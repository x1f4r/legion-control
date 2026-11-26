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
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';
import { SAMPLE_VIDEO_WEBM_BASE64 } from '../link/mock-media.js';
import {
  browserTest,
  launchBrowser,
  LEGION_BIN,
  startLegionServer,
  startStaticUiServer,
  waitForLayoutSettled,
  waitForMediaReady,
} from './browser-helpers.mjs';

browserTest(
  'Player: video controls, viewer Fullscreen API, controlled-clock timing, single-seek drag, queue, keys, resume, and picture immersive',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
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
            let guard = 0;
            while (guard < 10_000) {
              guard += 1;
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

      const page = tracker.attach(await context.newPage(), {
        label: 'video-controls',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      tracker.setStep('open Arrival.mp4');
      await page.goto(`${server.url}/?mock=browser-fs#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').waitFor();

      // Open first video (`Arrival.mp4`).
      await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
      const videoEl = page.locator('.viewer video.viewer-video[src]');
      await tracker.waitForLocator(videoEl, 'Arrival.mp4 video[src]');
      await waitForMediaReady(videoEl, 'Arrival.mp4 metadata', { tracker, minReadyState: 1 });
      await tracker.waitForFn(
        page,
        () => {
          const v = document.querySelector('.viewer video.viewer-video');
          return v && v.readyState >= 1 && Number.isFinite(v.duration) && v.duration > 1;
        },
        null,
        'Arrival.mp4 duration > 1s',
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
    tracker.setStep('Browser Fullscreen API on .viewer');
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await tracker.waitForFn(
      page,
      () => document.fullscreenElement === document.querySelector('.viewer'),
      null,
      'fullscreenElement === .viewer',
    );
    const fsTag = await page.evaluate(() => ({
      isViewer: document.fullscreenElement === document.querySelector('.viewer'),
      isVideo: document.fullscreenElement === document.querySelector('.viewer video'),
    }));
    assert.equal(fsTag.isViewer, true);
    assert.equal(fsTag.isVideo, false);

    // `F` leaves fullscreen while keeping the viewer open.
    await page.keyboard.press('f');
    await tracker.waitForFn(page, () => document.fullscreenElement === null, null, 'exit fullscreen via F');
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

    // 3b. Viewer is a modal: background file list is inert while open, and pressing Tab while the
    // overlay is hidden reveals the overlay and focuses its first control inside the viewer.
    const bgInertWhileOpen = await page.evaluate(() => Boolean(document.querySelector('.grid')?.closest('[inert]')));
    assert.equal(bgInertWhileOpen, true);
    await page.keyboard.press('Tab');
    const afterHiddenTab = await page.evaluate(() => {
      const overlay = document.querySelector('.player-overlay');
      const firstBtn = overlay.querySelector('button');
      const active = document.activeElement;
      return {
        hidden: overlay.classList.contains('hidden'),
        inert: overlay.inert,
        insideViewer: Boolean(document.querySelector('.viewer')?.contains(active)),
        focusedFirst: active === firstBtn,
      };
    });
    assert.equal(afterHiddenTab.hidden, false);
    assert.equal(afterHiddenTab.inert, false);
    assert.equal(afterHiddenTab.insideViewer, true);
    assert.equal(afterHiddenTab.focusedFirst, true);
    // Blur focus back out and let overlay hide again before testing stage taps below.
    await page.evaluate(async () => {
      document.activeElement?.blur?.();
      window.__LEGION_TEST_PLAYER_CLOCK__.advance(3000);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    });
    assert.equal(
      await page.evaluate(() => document.querySelector('.player-overlay').classList.contains('hidden')),
      true,
    );

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
    tracker.setStep('next video Beach.mp4 and URL revocation');
    const firstVideoUrl = await page.evaluate(() => document.querySelector('.viewer video.viewer-video')?.getAttribute('src') || '');
    assert.ok(firstVideoUrl.length > 0);
    await page.keyboard.press('n');
    await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
    await tracker.waitForFn(
      page,
      (prev) => {
        const cur = document.querySelector('.viewer video.viewer-video')?.getAttribute('src') || '';
        return Boolean(cur && cur !== prev && window.__LEGION_MOCK_REVOKED_URLS__?.includes(prev));
      },
      firstVideoUrl,
      'next video src swap and previous URL revoke',
    );
    const nextVideoUrl = await page.evaluate(() => document.querySelector('.viewer video.viewer-video')?.getAttribute('src') || '');
    assert.notEqual(nextVideoUrl, firstVideoUrl);
    assert.equal(await page.locator('.viewer button[aria-label="Previous video"]').count(), 1);
    assert.equal(await page.locator('.viewer button[aria-label="Next video"]').count(), 1);

    // `Esc` closes the viewer when not in fullscreen and revokes the active video URL, restoring background inert state.
    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    const bgInertAfterClose = await page.evaluate(() => Boolean(document.querySelector('.grid')?.closest('[inert]')));
    assert.equal(bgInertAfterClose, false);
    await tracker.waitForFn(
      page,
      (u) => Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(u),
      nextVideoUrl,
      'revocation of Beach.mp4 URL on close',
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
    await tracker.waitForFn(
      page,
      () =>
        document.querySelector('.viewer')?.classList.contains('chrome-hidden') &&
        window.__LEGION_MOCK_IMMERSIVE_CALLS__.some((c) => c.on === true && c.orientation === 'none'),
      null,
      'picture chrome-hidden and immersive(true)',
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

browserTest(
  'Player: instance and full screen survive file changes (next, prev, song end, Back, and keys during loading)',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'instance-survival',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

    // 1. Video full screen survives N and P: one immersive(true), zero immersive(false) in between,
    // same player DOM node stays mounted, Back leaves full screen on the CURRENT file, second Back closes.
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
    await page.locator('.viewer video.viewer-video[src]').waitFor();

    await page.evaluate(() => {
      window.__LEGION_MOCK_IMMERSIVE_CALLS__.length = 0;
      window.__LEGION_VIDEO_WRAP_REF__ = document.querySelector('.player-video-wrap');
    });

    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();

    await page.keyboard.press('n');
    await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
    await page.locator('.viewer video.viewer-video[src]').waitFor();

    await page.keyboard.press('p');
    await page.locator('.player-top .name', { hasText: 'Arrival.mp4' }).waitFor();
    await page.locator('.viewer video.viewer-video[src]').waitFor();

    await page.keyboard.press('n');
    await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
    await page.locator('.viewer video.viewer-video[src]').waitFor();

    const afterSkips = await page.evaluate(() => ({
      immersiveCalls: window.__LEGION_MOCK_IMMERSIVE_CALLS__.map((c) => c.on),
      sameInstance: document.querySelector('.player-video-wrap') === window.__LEGION_VIDEO_WRAP_REF__,
      stillFullscreen: Boolean(document.querySelector('.viewer button[aria-label="Exit full screen"]')),
    }));
    assert.deepEqual(afterSkips.immersiveCalls, [true]);
    assert.equal(afterSkips.sameInstance, true);
    assert.equal(afterSkips.stillFullscreen, true);

    // First Back leaves full screen and stays on Beach.mp4 (never reopens Arrival.mp4).
    await page.evaluate(() => window.history.back());
    await page.locator('.viewer button[aria-label="Full screen"]').waitFor({ timeout: 5000 });
    assert.equal(await page.locator('.player-top .name').textContent(), 'Beach.mp4');

    // Second Back closes the viewer cleanly.
    await page.evaluate(() => window.history.back());
    await page.locator('.viewer').waitFor({ state: 'hidden', timeout: 5000 });

    // 2. Keys pressed while the next file is still loading (gated contentUrl mint) are not lost.
    tracker.setStep('keys pressed while next video URL is minting');
    await page.goto(`${server.url}/?mock=slow-content#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
    await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'initial Arrival.mp4 src');
    await page.evaluate(() => {
      window.__LEGION_MOCK_IMMERSIVE_CALLS__.length = 0;
    });
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();

    // Hold Beach.mp4 contentUrl behind a deterministic gate so N -> P never races the mock timer.
    await page.evaluate(() => {
      let release = () => {};
      const promise = new Promise((r) => {
        release = r;
      });
      window.__TEST_CONTENT_GATE_RELEASE__ = release;
      window.__LEGION_MOCK_CONTENT_GATE__ = ({ path: p }) =>
        String(p || '').includes('Beach.mp4') ? promise : null;
    });
    try {
      await page.keyboard.press('n');
      await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
      const whileLoading = await page.evaluate(() => ({
        hasSrc: document.querySelector('.viewer video.viewer-video')?.hasAttribute('src'),
        hasSpinner: Boolean(document.querySelector('.player-spinner')),
      }));
      assert.equal(whileLoading.hasSrc, false);
      assert.equal(whileLoading.hasSpinner, true);
      await page.keyboard.press('p');
    } finally {
      await page.evaluate(() => {
        window.__TEST_CONTENT_GATE_RELEASE__?.();
        delete window.__TEST_CONTENT_GATE_RELEASE__;
        delete window.__LEGION_MOCK_CONTENT_GATE__;
      });
    }
    await page.locator('.player-top .name', { hasText: 'Arrival.mp4' }).waitFor();
    await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 src after P');
    const afterSlowSkip = await page.evaluate(() => ({
      immersiveCalls: window.__LEGION_MOCK_IMMERSIVE_CALLS__.map((c) => c.on),
      stillFullscreen: Boolean(document.querySelector('.viewer button[aria-label="Exit full screen"]')),
    }));
    assert.deepEqual(afterSlowSkip.immersiveCalls, [true]);
    assert.equal(afterSlowSkip.stillFullscreen, true);

    // 3. Music player keeps the exact same DOM instance across song end, next, and previous.
    tracker.setStep('music player DOM survival and Escape during audio URL mint');
    await page.goto(`${server.url}/?mock=slow-content#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).click();
    await page.locator('.music-screen').waitFor();
    await tracker.waitForLocator(page.locator('.viewer audio[src]'), 'first track audio[src]', { state: 'attached' });
    await page.evaluate(() => {
      window.__LEGION_MUSIC_NODE_REF__ = document.querySelector('.music-screen');
      document.querySelector('.viewer audio').dispatchEvent(new Event('ended'));
    });
    await page.locator('.music-title', { hasText: 'Rain on Glass' }).waitFor();
    const sameMusicNode = await page.evaluate(
      () => document.querySelector('.music-screen') === window.__LEGION_MUSIC_NODE_REF__,
    );
    assert.equal(sameMusicNode, true);

    // Hold the next track's audio URL mint so Escape definitely arrives mid-load.
    await page.evaluate(() => {
      let release = () => {};
      const promise = new Promise((r) => {
        release = r;
      });
      window.__TEST_CONTENT_GATE_RELEASE__ = release;
      window.__LEGION_MOCK_CONTENT_GATE__ = () => promise;
    });
    try {
      await page.keyboard.press('n');
      await page.keyboard.press('Escape');
      await tracker.waitForLocator(page.locator('.viewer'), 'viewer hidden after Escape during audio load', {
        state: 'hidden',
      });
    } finally {
      await page.evaluate(() => {
        window.__TEST_CONTENT_GATE_RELEASE__?.();
        delete window.__TEST_CONTENT_GATE_RELEASE__;
        delete window.__LEGION_MOCK_CONTENT_GATE__;
      });
    }

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

browserTest(
  'Player: single full-screen owner keeps Rename and Delete from full screen in sync',
  { timeout: 90_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'fs-owner',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

    // 1. Page boot calls immersive(false) and keepScreenOn(false) once to clear any stale native hold.
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').waitFor();
    const bootResets = await page.evaluate(() => ({
      immersive: window.__LEGION_MOCK_IMMERSIVE_CALLS__.map((c) => c.on),
      keepScreenOn: window.__LEGION_MOCK_KEEP_SCREEN_ON_CALLS__.map((c) => c.on),
    }));
    assert.deepEqual(bootResets.immersive, [false]);
    assert.deepEqual(bootResets.keepScreenOn, [false]);

    // 2. Rename from full screen (mock mode sends no native change event):
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
    await page.locator('.viewer video.viewer-video[src]').waitFor();

    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();

    await page.locator('.viewer .player-top button[aria-label="More options"]').click();
    await page.locator('.sheet button.row', { hasText: 'Rename' }).click();
    await page.locator('.sheet h2', { hasText: 'Rename' }).waitFor({ timeout: 5000 });
    await page.locator('.sheet button', { hasText: 'Cancel' }).click();
    await page.locator('.sheet').waitFor({ state: 'hidden', timeout: 5000 });

    // Player button must already say "Full screen" (NOT stale "Exit full screen").
    await page.locator('.viewer button[aria-label="Full screen"]').waitFor({ timeout: 5000 });
    assert.equal(await page.locator('.viewer button[aria-label="Exit full screen"]').count(), 0);

    // A SINGLE Back must close the viewer (no leftover full-screen history entry!).
    await page.evaluate(() => window.history.back());
    await page.locator('.viewer').waitFor({ state: 'hidden', timeout: 5000 });

    // 2. Delete from full screen: confirming Delete closes the viewer cleanly instead of staying open on the deleted file.
    await page.locator('.grid a.cell[aria-label="Beach.mp4"]').click();
    await page.locator('.viewer video.viewer-video[src]').waitFor();

    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();

    await page.locator('.viewer .player-top button[aria-label="More options"]').click();
    await page.locator('.sheet button.row', { hasText: 'Delete' }).click();
    await page.locator('.sheet h2', { hasText: 'Delete Beach.mp4?' }).waitFor({ timeout: 5000 });
    await page.locator('.sheet button.btn.danger', { hasText: 'Delete' }).click();

    await page.locator('.viewer').waitFor({ state: 'hidden', timeout: 5000 });
    await page.locator('.sheet').waitFor({ state: 'hidden', timeout: 5000 });
    await tracker.waitForFn(
      page,
      () => !document.querySelector('.grid a.cell[aria-label="Beach.mp4"]'),
      null,
      'Beach.mp4 removed from grid after delete',
    );

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

browserTest(
  'Player: music screen, tags, covers in listing, MediaSession, auto-advance, large library, and reduced motion',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({
        viewport: { width: 1100, height: 720 },
        colorScheme: 'dark',
      });
      const page = tracker.attach(await context.newPage(), {
        label: 'music-screen',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

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
      const coverBox = document.querySelector('.music-cover-box');
      const coverRect = document.querySelector('.music-cover-wrap').getBoundingClientRect();
      const panelRect = document.querySelector('.music-meta-panel').getBoundingClientRect();
      return {
        nativeControls: viewer.querySelectorAll('audio[controls]').length,
        disabledCount: viewer.querySelectorAll('[disabled], [aria-disabled="true"]').length,
        coverLeftOfPanel: coverRect.x + coverRect.width <= panelRect.x + 8,
        coverHairlineShadow: getComputedStyle(coverBox, '::after').boxShadow,
        mediaSessionTitle: navigator.mediaSession?.metadata?.title || '',
        mediaSessionArtist: navigator.mediaSession?.metadata?.artist || '',
        mediaSessionAlbum: navigator.mediaSession?.metadata?.album || '',
      };
    });
    assert.equal(musicAudit.nativeControls, 0);
    assert.equal(musicAudit.disabledCount, 0);
    assert.equal(musicAudit.coverLeftOfPanel, true);
    assert.match(musicAudit.coverHairlineShadow, /inset/i);
    assert.match(musicAudit.coverHairlineShadow, /1px/);
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
    tracker.setStep('music speed 1.25x in More sheet');
    await page.locator('.music-top button[aria-label="More options"]').click();
    await page.locator('.sheet .music-speed-chips button', { hasText: '1.25×' }).click();
    await tracker.waitForFn(
      page,
      () => document.querySelector('.viewer audio')?.playbackRate === 1.25,
      null,
      'audio playbackRate === 1.25',
    );
    await page.keyboard.press('Escape');
    await page.locator('.sheet').waitFor({ state: 'hidden' });

    // Ending a song auto-advances to the next track (`02 - Rain on Glass.mp3`), revokes the previous URL,
    // and keeps the Play button at the exact same horizontal position.
    tracker.setStep('music auto-advance on ended');
    const firstSongUrl = await page.evaluate(() => document.querySelector('.viewer audio')?.getAttribute('src') || '');
    assert.ok(firstSongUrl.length > 0);
    await page.evaluate(() => {
      document.querySelector('.viewer audio').dispatchEvent(new Event('ended'));
    });
    await page.locator('.music-title', { hasText: 'Rain on Glass' }).waitFor();
    const midPlayX = await page.evaluate(() => document.querySelector('.music-play-btn').getBoundingClientRect().x);
    assert.ok(Math.abs(midPlayX - firstPlayBox.x) < 0.5, `Play button shifted: ${firstPlayBox.x} -> ${midPlayX}`);
    const afterAutoNext = await page.evaluate((prev) => ({
      revokedPrev: Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(prev),
      nextUrl: document.querySelector('.viewer audio')?.getAttribute('src') || '',
    }), firstSongUrl);
    assert.equal(afterAutoNext.revokedPrev, true, 'Auto-advancing to next song must revoke previous content URL');
    assert.notEqual(afterAutoNext.nextUrl, firstSongUrl);

    // Ensure track is playing -> MediaSession reflects "playing"; closing the music player clears
    // metadata to null and playbackState to "none".
    tracker.setStep('MediaSession playing state');
    const playBtnLabel = await page.locator('.music-play-btn').getAttribute('aria-label');
    if (playBtnLabel === 'Play') {
      await page.locator('.music-play-btn').click();
    }
    await tracker.waitForFn(
      page,
      () => navigator.mediaSession?.playbackState === 'playing',
      null,
      'MediaSession playbackState === playing',
    );
    await page.locator('.music-top button[aria-label="Close"]').click();
    await page.locator('.viewer').waitFor({ state: 'hidden' });
    const clearedSession = await page.evaluate(() => ({
      metadata: navigator.mediaSession?.metadata ?? null,
      playbackState: navigator.mediaSession?.playbackState || '',
    }));
    assert.equal(clearedSession.metadata, null);
    assert.equal(clearedSession.playbackState, 'none');

    // Open untagged song (last song in folder): title is filename without extension, no subtitle,
    // no "Unknown artist", plain bg, muted tile (NOT green), Next absent while Play stays in exact same slot!
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

    // 3. Large library (300 songs) with gated covers: title appears BEFORE cover loads.
    tracker.setStep('300-song library title before slow cover');
    await page.goto(`${server.url}/?mock=big-music,slow-cover#/files/tower/%2Fhome%2Fme%2FMusic%2FLibrary300`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: 'Track-001.mp3' }).waitFor();
    await page.evaluate(() => {
      let release = () => {};
      const promise = new Promise((r) => {
        release = r;
      });
      window.__TEST_COVER_GATE_RELEASE__ = release;
      window.__LEGION_MOCK_COVER_GATE__ = () => promise;
    });
    try {
      await page.locator('.list a.row', { hasText: 'Track-001.mp3' }).click();
      await page.locator('.music-title').waitFor();
      const immediateCheck = await page.evaluate(() => ({
        title: document.querySelector('.music-title')?.textContent?.trim() || '',
        hasCoverAlready: Boolean(document.querySelector('img.music-cover')),
      }));
      assert.ok(immediateCheck.title.startsWith('Track'), `Expected title immediately, got "${immediateCheck.title}"`);
      assert.equal(immediateCheck.hasCoverAlready, false, 'Expected title to render before slow cover resolves');
    } finally {
      await page.evaluate(() => {
        window.__TEST_COVER_GATE_RELEASE__?.();
        delete window.__TEST_COVER_GATE_RELEASE__;
        delete window.__LEGION_MOCK_COVER_GATE__;
      });
    }
    await tracker.waitForLocator(page.locator('img.music-cover'), 'Track-001 cover image after gate release');

    // 4. Reduced motion disables overlay transitions.
    tracker.setStep('reduced motion overlay transition check');
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

browserTest(
  'Player: real legion serve HTTP door enters Fullscreen API on viewer element with zero disabled controls',
  { timeout: 90_000 },
  async (t, tracker) => {
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

    const server = await startLegionServer(legionHome, userHome);
    tracker.setServerStderr(server.getStderr);
    const url = server.url;

    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'real-http-player',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      tracker.setStep('open clip.webm on real HTTP door');
      await page.goto(`${url}/#token=${encodeURIComponent(tokenOut.token)}`, { waitUntil: 'domcontentloaded' });
      await page.locator('.nav a[href="#/files"]').waitFor();
      await page.goto(`${url}/#/files/tower/${encodeURIComponent(mediaDir)}`, { waitUntil: 'domcontentloaded' });
      const clipEntry = page.locator('a.row:has-text("clip.webm"), a.cell[aria-label="clip.webm"]');
      await clipEntry.waitFor();
      await clipEntry.click();
      await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'clip.webm video[src]');

      // Real HTTP door (`http.js`): Fullscreen API targets `.viewer` (not `<video>`) and no control is disabled.
      assert.equal(await page.locator('.viewer [disabled]').count(), 0);
      assert.equal(await page.locator('.viewer video[controls]').count(), 0);
      await page.locator('.viewer button[aria-label="Full screen"]').click();
      await tracker.waitForFn(
        page,
        () => document.fullscreenElement === document.querySelector('.viewer'),
        null,
        'fullscreenElement === .viewer on real HTTP door',
      );
      const fsInfo = await page.evaluate(() => ({
        isViewer: document.fullscreenElement === document.querySelector('.viewer'),
        isVideo: document.fullscreenElement === document.querySelector('.viewer video'),
      }));
      assert.equal(fsInfo.isViewer, true);
      assert.equal(fsInfo.isVideo, false);

      await page.locator('.viewer button[aria-label="Exit full screen"]').click();
      await tracker.waitForFn(page, () => document.fullscreenElement === null, null, 'exit fullscreen on real HTTP door');
      assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
      await context.close();
    } finally {
      await browser.close();
      await server.stop();
      fs.rmSync(root, { recursive: true, force: true });
    }
  },
);

browserTest(
  'Player: hiding the desktop window pauses video and music in place and leaves full screen',
  { timeout: 90_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'window-hidden',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

    // 1. Video playing in full screen -> window hides -> paused right where it was, no longer in full screen.
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
    await page.locator('.viewer video.viewer-video[src]').waitFor({ timeout: 5000 });
    await tracker.waitForFn(
      page,
      () => {
        const v = document.querySelector('.viewer video');
        return v && v.readyState >= 1 && Number.isFinite(v.duration) && v.duration > 0;
      },
      null,
      'Arrival.mp4 metadata before hide',
    );
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();
    const targetVideoTime = await page.evaluate(async () => {
      const v = document.querySelector('.viewer video');
      const t = Math.min(0.4, Math.max(0.1, v.duration * 0.35));
      v.currentTime = t;
      await v.play();
      return t;
    });
    await tracker.waitForFn(
      page,
      () => {
        const v = document.querySelector('.viewer video');
        return v && !v.paused;
      },
      null,
      'video playing before window hide',
    );

    await page.evaluate(() => window.__LEGION_MOCK_TRIGGER_WINDOW_HIDDEN__());
    await page.locator('.viewer button[aria-label="Full screen"]').waitFor();
    assert.equal(await page.locator('.viewer button[aria-label="Exit full screen"]').count(), 0);
    const videoAfterHide = await page.evaluate(() => {
      const v = document.querySelector('.viewer video');
      const keepCalls = window.__LEGION_MOCK_KEEP_SCREEN_ON_CALLS__ || [];
      return {
        paused: v?.paused,
        currentTime: v?.currentTime ?? 0,
        lastKeepScreenOn: keepCalls.length ? keepCalls[keepCalls.length - 1].on : null,
      };
    });
    assert.equal(videoAfterHide.paused, true);
    assert.ok(
      videoAfterHide.currentTime >= targetVideoTime - 0.05,
      `Video lost position on hide: ${videoAfterHide.currentTime} < ${targetVideoTime}`,
    );
    assert.equal(videoAfterHide.lastKeepScreenOn, false);

    // 2. Music playing -> window hides -> paused in place with Play button shown.
    tracker.setStep('music pauses in place on window hide');
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).click();
    await tracker.waitForLocator(page.locator('.viewer audio[src]'), 'music audio[src]', { state: 'attached' });
    await tracker.waitForFn(
      page,
      () => {
        const a = document.querySelector('.viewer audio');
        return a && a.readyState >= 1 && Number.isFinite(a.duration) && a.duration > 0;
      },
      null,
      'music metadata before hide',
    );
    const targetAudioTime = await page.evaluate(async () => {
      const a = document.querySelector('.viewer audio');
      const t = Math.min(0.25, Math.max(0.05, a.duration * 0.3));
      a.currentTime = t;
      await a.play();
      return t;
    });
    await tracker.waitForFn(
      page,
      () => {
        const a = document.querySelector('.viewer audio');
        return a && !a.paused;
      },
      null,
      'audio playing before window hide',
    );
    await page.evaluate(() => window.__LEGION_MOCK_TRIGGER_WINDOW_HIDDEN__());
    await page.locator('.music-play-btn[aria-label="Play"]').waitFor();
    const audioAfterHide = await page.evaluate(() => {
      const a = document.querySelector('.viewer audio');
      return { paused: a?.paused, currentTime: a?.currentTime ?? 0 };
    });
    assert.equal(audioAfterHide.paused, true);
    assert.ok(
      audioAfterHide.currentTime >= targetAudioTime - 0.05,
      `Audio lost position on hide: ${audioAfterHide.currentTime} < ${targetAudioTime}`,
    );

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

browserTest(
  'Player: Space play/pause intent pressed while next file loads applies once ready',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'space-intent',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      const withContentGate = async (matchSubstr, fn) => {
        await page.evaluate((needle) => {
          let release = () => {};
          const promise = new Promise((r) => {
            release = r;
          });
          window.__TEST_CONTENT_GATE_RELEASE__ = release;
          window.__LEGION_MOCK_CONTENT_GATE__ = ({ path: p }) =>
            !needle || String(p || '').includes(needle) ? promise : null;
        }, matchSubstr);
        try {
          await fn();
        } finally {
          await page.evaluate(() => {
            window.__TEST_CONTENT_GATE_RELEASE__?.();
            delete window.__TEST_CONTENT_GATE_RELEASE__;
            delete window.__LEGION_MOCK_CONTENT_GATE__;
          });
        }
      };

      // 1. Video paused -> N -> press Space while Beach.mp4 is still minting (no src) -> auto-plays when ready!
      tracker.setStep('video Space play intent during gated load');
      await page.goto(`${server.url}/?mock=slow-content#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
      await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 video[src]');
      await tracker.waitForFn(
        page,
        () => {
          const v = document.querySelector('.viewer video');
          return v && v.readyState >= 1 && v.paused;
        },
        null,
        'Arrival.mp4 ready and paused',
      );

      await withContentGate('Beach.mp4', async () => {
        await page.keyboard.press('n');
        await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
        assert.equal(
          await page.evaluate(() => document.querySelector('.viewer video')?.hasAttribute('src')),
          false,
        );
        await page.keyboard.press('Space');
      });
      await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'Beach.mp4 video[src]');
      await tracker.waitForFn(
        page,
        () => {
          const v = document.querySelector('.viewer video');
          return v && v.readyState >= 1 && !v.paused;
        },
        null,
        'Beach.mp4 playing after Space-during-load',
      );

      // 2. Video playing -> P back to Arrival.mp4 -> press Space while loading to PAUSE -> stays paused when ready!
      tracker.setStep('video Space pause intent during gated load');
      await withContentGate('Arrival.mp4', async () => {
        await page.keyboard.press('p');
        await page.locator('.player-top .name', { hasText: 'Arrival.mp4' }).waitFor();
        assert.equal(
          await page.evaluate(() => document.querySelector('.viewer video')?.hasAttribute('src')),
          false,
        );
        await page.keyboard.press('Space');
      });
      await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 video[src] after P');
      await tracker.waitForFn(
        page,
        () => {
          const v = document.querySelector('.viewer video');
          return v && v.readyState >= 1;
        },
        null,
        'Arrival.mp4 readyState >= 1 after P',
      );
      await tracker.waitForLocator(page.locator('.player-spinner'), 'loading spinner detached after Arrival.mp4 metadata', {
        state: 'detached',
      });
      await waitForLayoutSettled(page, 2);
      assert.equal(
        await page.evaluate(() => document.querySelector('.viewer video')?.paused),
        true,
        'Space pressed during load while playing must pause once next video loads',
      );

      // 3. Music paused -> N -> press Space while next track URL is minting -> auto-plays when ready!
      tracker.setStep('music Space play intent during gated load');
      await page.goto(`${server.url}/?mock=slow-content#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).click();
      await tracker.waitForLocator(page.locator('.viewer audio[src]'), 'first track audio[src]', { state: 'attached' });
      await tracker.waitForFn(
        page,
        () => {
          const a = document.querySelector('.viewer audio');
          return a && a.readyState >= 1 && a.paused;
        },
        null,
        'first track ready and paused',
      );
      await withContentGate('Rain on Glass', async () => {
        await page.keyboard.press('n');
        await page.locator('.music-title', { hasText: 'Rain on Glass' }).waitFor();
        assert.equal(
          await page.evaluate(() => document.querySelector('.viewer audio')?.hasAttribute('src')),
          false,
        );
        await page.keyboard.press('Space');
      });
      await tracker.waitForLocator(page.locator('.viewer audio[src]'), 'Rain on Glass audio[src]', {
        state: 'attached',
      });
      await tracker.waitForFn(
        page,
        () => {
          const a = document.querySelector('.viewer audio');
          return a && a.readyState >= 1 && !a.paused;
        },
        null,
        'Rain on Glass playing after Space-during-load',
      );

      assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
      await context.close();
    } finally {
      await browser.close();
      await server.stop();
    }
  },
);

browserTest(
  'Player: playback started before loadedmetadata fires is not paused by VideoPlayer or MusicPlayer',
  { timeout: 90_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'late-loadedmetadata',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      // 1. VideoPlayer: playback started before a delayed loadedmetadata event must keep playing.
      tracker.setStep('VideoPlayer preserves active playback when loadedmetadata fires late');
      await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
      await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 video[src]');
      await waitForMediaReady(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 ready', {
        tracker,
        minReadyState: 1,
      });
      await page.evaluate(async () => {
        const v = document.querySelector('.viewer video');
        await v.play();
        // Simulate a queued loadedmetadata event firing on the main thread after play() has started.
        v.dispatchEvent(new Event('loadedmetadata'));
      });
      await waitForLayoutSettled(page, 2);
      const videoState = await page.evaluate(() => ({
        paused: document.querySelector('.viewer video')?.paused,
        hasPauseBtn: Boolean(document.querySelector('.viewer button[aria-label="Pause"]')),
      }));
      assert.equal(videoState.paused, false, 'VideoPlayer.onLoadedMetadata must not pause started playback');
      assert.equal(videoState.hasPauseBtn, true, 'VideoPlayer UI must reflect playing state after late loadedmetadata');

      // 2. MusicPlayer: playback started before a delayed loadedmetadata event must keep playing.
      tracker.setStep('MusicPlayer preserves active playback when loadedmetadata fires late');
      await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('.list a.row', { hasText: '01 - Morning Light.flac' }).click();
      await tracker.waitForLocator(page.locator('.viewer audio[src]'), 'Morning Light audio[src]', {
        state: 'attached',
      });
      await waitForMediaReady(page.locator('.viewer audio[src]'), 'Morning Light ready', {
        tracker,
        minReadyState: 1,
      });
      await page.evaluate(async () => {
        const a = document.querySelector('.viewer audio');
        await a.play();
        a.dispatchEvent(new Event('loadedmetadata'));
      });
      await waitForLayoutSettled(page, 2);
      const audioState = await page.evaluate(() => ({
        paused: document.querySelector('.viewer audio')?.paused,
        hasPauseBtn: Boolean(document.querySelector('.music-play-btn[aria-label="Pause"]')),
      }));
      assert.equal(audioState.paused, false, 'MusicPlayer.onLoadedMetadata must not pause started playback');
      assert.equal(audioState.hasPauseBtn, true, 'MusicPlayer UI must reflect playing state after late loadedmetadata');

      assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
      await context.close();
    } finally {
      await browser.close();
      await server.stop();
    }
  },
);

browserTest(
  'Player: reaching an unplayable file in full screen keeps top bar, exit full screen, and N/P queue keys working',
  { timeout: 90_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'unplayable-fs',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.grid a.cell[aria-label="Beach.mp4"]').click();
    await page.locator('.viewer video.viewer-video[src]').waitFor({ timeout: 5000 });
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor({ timeout: 3000 });

    // Press N in full screen -> lands on unplayable `Concert (H.264).mp4` error fallback screen.
    await page.keyboard.press('n');
    await page.locator('.viewer-top .name', { hasText: 'Concert (H.264).mp4' }).waitFor({ timeout: 5000 });
    await page.locator('.viewer-stage .said').waitFor({ timeout: 5000 });

    // Top bar has Back, name, Exit full screen, and More options (not stranded in full screen!).
    assert.equal(await page.locator('.viewer-top button[aria-label="Back"]').count(), 1);
    assert.equal(await page.locator('.viewer-top button[aria-label="Exit full screen"]').count(), 1);
    assert.equal(await page.locator('.viewer-top button[aria-label="More options"]').count(), 1);

    // P navigates back to Beach.mp4 while staying in full screen (queue not stranded).
    await page.keyboard.press('p');
    await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor({ timeout: 5000 });
    await page.locator('.viewer video.viewer-video[src]').waitFor({ timeout: 5000 });
    assert.equal(await page.locator('.viewer button[aria-label="Exit full screen"]').count(), 1);

    // Return to unplayable file in full screen and verify Exit full screen button leaves full screen cleanly.
    await page.keyboard.press('n');
    await page.locator('.viewer-stage .said').waitFor({ timeout: 5000 });
    await page.locator('.viewer-top button[aria-label="Exit full screen"]').click();
    await page.locator('.viewer-top button[aria-label="Close"]').waitFor({ timeout: 3000 });
    assert.equal(await page.locator('.viewer-top button[aria-label="Exit full screen"]').count(), 0);
    // Still on the error screen (did not close viewer); Escape now closes viewer cleanly.
    assert.equal(await page.locator('.viewer-stage .said').count(), 1);
    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'hidden', timeout: 3000 });

    // Also verify Escape on unplayable file while in full screen exits full screen FIRST before closing.
    await page.locator('.grid a.cell[aria-label="Beach.mp4"]').click();
    await page.locator('.viewer video.viewer-video[src]').waitFor({ timeout: 5000 });
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor({ timeout: 3000 });
    await page.keyboard.press('n');
    await page.locator('.viewer-stage .said').waitFor({ timeout: 5000 });
    await page.keyboard.press('Escape');
    await page.locator('.viewer-top button[aria-label="Close"]').waitFor({ timeout: 3000 });
    assert.equal(await page.locator('.viewer').count(), 1);

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});


browserTest(
  'Player: deleting a picture, full-screen video, music track, or deep-linked file from the viewer leaves no history entry for it',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      // 1. Folder -> Picture -> More -> Delete -> Confirm -> system Back leaves folder (never reopens deleted picture).
      {
        tracker.setStep('delete picture from viewer and check Back history');
        const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: 'delete-picture',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme`, { waitUntil: 'domcontentloaded' });
        await page.locator('a.row', { hasText: 'Videos' }).first().click();
        await page.locator('a.row', { hasText: 'Holiday' }).first().click();
        await page.locator('.grid a.cell[aria-label="IMG_2041.jpg"]').waitFor();
        const holidayHash = await page.evaluate(() => window.location.hash);

        await page.locator('.grid a.cell[aria-label="IMG_2041.jpg"]').click();
        await page.locator('.viewer').waitFor();
        await page.locator('.viewer button[aria-label="More options"]').click();
        await page.locator('.sheet button', { hasText: 'Delete' }).click();
        await page.locator('.sheet button.danger', { hasText: 'Delete' }).click();
        await tracker.waitForLocator(page.locator('.viewer'), 'viewer detached after picture delete', {
          state: 'detached',
        });
        await tracker.waitForFn(
          page,
          (expected) => window.location.hash === expected && !document.querySelector('.grid a.cell[aria-label="IMG_2041.jpg"]'),
          holidayHash,
          'hash restored and IMG_2041.jpg detached',
        );

        assert.equal(await page.evaluate(() => window.location.hash), holidayHash);
        assert.equal(await page.locator('.grid a.cell[aria-label="IMG_2041.jpg"]').count(), 0);

        // System Back must go from Holiday to Videos, never reopening the viewer on the deleted file.
        await page.evaluate(() => window.history.back());
        await tracker.waitForFn(
          page,
          () => decodeURIComponent(window.location.hash).endsWith('/home/me/Videos'),
          null,
          'Back to /home/me/Videos after picture delete',
        );
        assert.equal(await page.locator('.viewer').count(), 0);

        // Another Back leaves Videos cleanly for /home/me.
        await page.evaluate(() => window.history.back());
        await tracker.waitForFn(
          page,
          () => decodeURIComponent(window.location.hash).endsWith('/home/me'),
          null,
          'Back to /home/me after picture delete',
        );
        assert.equal(await page.locator('.viewer').count(), 0);
        await context.close();
      }

      // 2. Folder -> Video -> Full screen -> Next sibling -> More -> Delete -> Confirm -> system Back leaves folder.
      {
        tracker.setStep('delete fullscreen video from viewer and check Back history');
        const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: 'delete-video',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos`, { waitUntil: 'domcontentloaded' });
        await page.locator('a.row', { hasText: 'Holiday' }).first().click();
        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').waitFor();
        const holidayHash = await page.evaluate(() => window.location.hash);

        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
        await page.locator('.viewer video.viewer-video[src]').waitFor();
        await page.locator('.viewer button[aria-label="Full screen"]').click();
        await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();

        // Step to Beach.mp4 while in full screen, then delete it via More -> Delete.
        await page.keyboard.press('n');
        await page.locator('.player-top .name', { hasText: 'Beach.mp4' }).waitFor();
        await page.locator('.viewer button[aria-label="More options"]').click();
        await page.locator('.sheet button', { hasText: 'Delete' }).click();
        await page.locator('.sheet button.danger', { hasText: 'Delete' }).click();
        await tracker.waitForLocator(page.locator('.viewer'), 'viewer detached after video delete', {
          state: 'detached',
        });
        await tracker.waitForFn(
          page,
          (expected) => window.location.hash === expected && !document.querySelector('.grid a.cell[aria-label="Beach.mp4"]'),
          holidayHash,
          'hash restored and Beach.mp4 detached',
        );

        assert.equal(await page.evaluate(() => window.location.hash), holidayHash);
        assert.equal(await page.locator('.grid a.cell[aria-label="Beach.mp4"]').count(), 0);

        await page.evaluate(() => window.history.back());
        await tracker.waitForFn(
          page,
          () => decodeURIComponent(window.location.hash).endsWith('/home/me/Videos'),
          null,
          'Back to /home/me/Videos after video delete',
        );
        assert.equal(await page.locator('.viewer').count(), 0);
        await context.close();
      }

      // 3. Folder -> Music -> More -> Delete -> Confirm -> system Back leaves folder.
      {
        tracker.setStep('delete music track from viewer and check Back history');
        const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: 'delete-music',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic`, { waitUntil: 'domcontentloaded' });
        await page.locator('a.row', { hasText: 'Album' }).first().click();
        await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).waitFor();
        const albumHash = await page.evaluate(() => window.location.hash);

        await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).click();
        await page.locator('.viewer audio[src]').waitFor({ state: 'attached' });
        await page.locator('.viewer button[aria-label="More options"]').click();
        await page.locator('.sheet button', { hasText: 'Delete' }).click();
        await page.locator('.sheet button.danger', { hasText: 'Delete' }).click();
        await tracker.waitForLocator(page.locator('.viewer'), 'viewer detached after music delete', {
          state: 'detached',
        });
        await tracker.waitForFn(
          page,
          (expected) =>
            window.location.hash === expected &&
            ![...document.querySelectorAll('a.row')].some((r) => r.textContent?.includes('01 - Morning Light.flac')),
          albumHash,
          'hash restored and 01 - Morning Light.flac detached',
        );

        assert.equal(await page.evaluate(() => window.location.hash), albumHash);
        assert.equal(await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).count(), 0);

        await page.evaluate(() => window.history.back());
        await tracker.waitForFn(
          page,
          () => decodeURIComponent(window.location.hash).endsWith('/home/me/Music'),
          null,
          'Back to /home/me/Music after track delete',
        );
        assert.equal(await page.locator('.viewer').count(), 0);
        await context.close();
      }

      // 4. Deep-linked viewer -> Delete -> Confirm -> viewer replaced in place with folder; Back goes to prior page.
      {
        tracker.setStep('delete deep-linked file from viewer and check Back history');
        const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: 'delete-deeplink',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos`, { waitUntil: 'domcontentloaded' });
        await page.locator('a.row', { hasText: 'Holiday' }).waitFor();
        await page.goto(
          `${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday?view=${encodeURIComponent('/home/me/Videos/Holiday/IMG_2042.jpg')}`,
          { waitUntil: 'domcontentloaded' },
        );
        await page.locator('.viewer').waitFor();
        await page.locator('.viewer button[aria-label="More options"]').click();
        await page.locator('.sheet button', { hasText: 'Delete' }).click();
        await page.locator('.sheet button.danger', { hasText: 'Delete' }).click();
        await tracker.waitForLocator(page.locator('.viewer'), 'viewer detached after deep-link delete', {
          state: 'detached',
        });
        await tracker.waitForFn(
          page,
          () =>
            decodeURIComponent(window.location.hash) === '#/files/tower//home/me/Videos/Holiday' &&
            !document.querySelector('.grid a.cell[aria-label="IMG_2042.jpg"]'),
          null,
          'deep-link hash replaced with Holiday folder and IMG_2042.jpg detached',
        );

        assert.equal(
          decodeURIComponent(await page.evaluate(() => window.location.hash)),
          '#/files/tower//home/me/Videos/Holiday',
        );
        assert.equal(await page.locator('.grid a.cell[aria-label="IMG_2042.jpg"]').count(), 0);

        await page.evaluate(() => window.history.back());
        await tracker.waitForFn(
          page,
          () => decodeURIComponent(window.location.hash).endsWith('/home/me/Videos'),
          null,
          'Back to /home/me/Videos after deep-link delete',
        );
        assert.equal(await page.locator('.viewer').count(), 0);
        await context.close();
      }

      assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    } finally {
      await browser.close();
      await server.stop();
    }
  },
);

browserTest(
  'Player: window-hidden pause stays resumable across loopback teardown, mints one fresh URL on play, and never loops on a dead URL',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const sampleVideoBuf = Buffer.from(SAMPLE_VIDEO_WEBM_BASE64, 'base64');
    let deadGeneration = 0;
    let serveBrokenFresh = false;
    const ticketHits = new Map();

    // Serve UI plus a controllable `/test-media/<gen>/clip.webm` loopback endpoint so we can
    // simulate a loopback listener teardown that kills generation 1 while keeping generation 2 alive.
    const server = await startStaticUiServer({
      extraHandler: (req, res) => {
        const parsed = new URL(req.url || '/', 'http://127.0.0.1');
        const mediaMatch = /^\/test-media\/(\d+)\/clip\.webm$/.exec(parsed.pathname);
        if (!mediaMatch) return false;
        const gen = Number(mediaMatch[1]);
        ticketHits.set(gen, (ticketHits.get(gen) || 0) + 1);
        if (gen <= deadGeneration || (serveBrokenFresh && gen >= 2)) {
          res.writeHead(410, { 'Content-Type': 'text/plain' });
          res.end('Gone');
          return true;
        }
        const range = req.headers.range;
        if (range) {
          const m = /^bytes=(\d+)-(\d*)$/.exec(range);
          const start = m ? Number(m[1]) : 0;
          const end = m && m[2] ? Math.min(Number(m[2]), sampleVideoBuf.length - 1) : sampleVideoBuf.length - 1;
          const slice = sampleVideoBuf.subarray(start, end + 1);
          res.writeHead(206, {
            'Content-Type': 'video/webm',
            'Accept-Ranges': 'bytes',
            'Content-Range': `bytes ${start}-${end}/${sampleVideoBuf.length}`,
            'Content-Length': String(slice.length),
          });
          if (req.method === 'HEAD') res.end();
          else res.end(slice);
          return true;
        }
        res.writeHead(200, {
          'Content-Type': 'video/webm',
          'Accept-Ranges': 'bytes',
          'Content-Length': String(sampleVideoBuf.length),
        });
        if (req.method === 'HEAD') res.end();
        else res.end(sampleVideoBuf);
        return true;
      },
    });
    const baseUrl = server.url;

    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      // 1. Playing video -> desktop window hides -> loopback tears down old URL -> stays paused
      //    without retrying while hidden -> pressing Play mints one fresh URL and resumes playing.
      {
        tracker.setStep('loopback teardown while hidden resumes with one fresh URL');
        const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
        await context.addInitScript((origin) => {
          window.__TEST_MINTS__ = [];
          window.__TEST_REVOKES__ = [];
          window.__TEST_GEN__ = 1;
          window.__LEGION_MOCK_OVERRIDE_CONTENT_URL__ = async ({ path: p }) => {
            if (String(p || '').endsWith('.mp4')) {
              const gen = window.__TEST_GEN__++;
              const url = `${origin}/test-media/${gen}/clip.webm`;
              window.__TEST_MINTS__.push(url);
              return { url, expires_in: 120 };
            }
            return null;
          };
          window.__LEGION_MOCK_ON_REVOKE_CONTENT_URL__ = (url) => {
            window.__TEST_REVOKES__.push(url);
          };
        }, baseUrl);

        const page = tracker.attach(await context.newPage(), {
          label: 'teardown-resume',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${baseUrl}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
        await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'gen 1 video[src]');
        await tracker.waitForFn(
          page,
          () => {
            const v = document.querySelector('.viewer video');
            return Boolean(v && v.readyState >= 1);
          },
          null,
          'gen 1 video readyState >= 1',
        );

        // Play to ~1.2 s so we verify position preservation across teardown + fresh mint.
        await page.evaluate(async () => {
          const v = document.querySelector('.viewer video');
          v.currentTime = 1.2;
          await v.play();
        });
        await tracker.waitForFn(
          page,
          () => {
            const v = document.querySelector('.viewer video');
            return Boolean(v && !v.paused && v.currentTime >= 1.1);
          },
          null,
          'gen 1 playing past 1.1s',
        );
        assert.equal(await page.evaluate(() => window.__TEST_MINTS__.length), 1);

        // Hide desktop window -> pauses in place, then simulate loopback teardown killing gen 1
        // and firing an aborted-stream error on the paused video element.
        await page.evaluate(() => window.__LEGION_MOCK_TRIGGER_WINDOW_HIDDEN__?.());
        deadGeneration = 1;
        await page.evaluate(() => {
          const v = document.querySelector('.viewer video');
          v.dispatchEvent(new Event('error'));
        });
        await waitForLayoutSettled(page, 4);

        // Must NOT auto-mint while hidden/paused and must NOT unmount into an error screen.
        assert.equal(await page.evaluate(() => window.__TEST_MINTS__.length), 1);
        assert.equal(await page.locator('.viewer video.viewer-video').count(), 1);
        assert.equal(await page.locator('.viewer-stage .said').count(), 0);
        assert.equal(await page.evaluate(() => document.querySelector('.viewer video')?.paused), true);

        // User returns and presses Space (Play) -> mints ONE fresh URL (gen 2), revokes gen 1,
        // seeks back near 1.2 s, and resumes playback.
        await page.keyboard.press('Space');
        await tracker.waitForFn(
          page,
          () => {
            const v = document.querySelector('.viewer video');
            return Boolean(
              v &&
                v.getAttribute('src')?.includes('/test-media/2/clip.webm') &&
                v.readyState >= 1 &&
                !v.paused,
            );
          },
          null,
          'gen 2 video playing after Space resume',
        );
        const stateAfterResume = await page.evaluate(() => {
          const v = document.querySelector('.viewer video');
          return {
            mints: window.__TEST_MINTS__.length,
            revokes: window.__TEST_REVOKES__,
            currentTime: v?.currentTime ?? 0,
            paused: v?.paused ?? true,
          };
        });
        assert.equal(stateAfterResume.mints, 2);
        assert.ok(
          stateAfterResume.revokes.some((u) => u.includes('/test-media/1/clip.webm')),
          `Expected dead URL gen 1 to be revoked, got ${JSON.stringify(stateAfterResume.revokes)}`,
        );
        assert.ok(stateAfterResume.currentTime >= 1.0, `Expected resumed time near 1.2s, got ${stateAfterResume.currentTime}`);
        assert.equal(stateAfterResume.paused, false);
        await context.close();
      }

      // 2. If the freshly minted URL after teardown ALSO fails (e.g. server still down), stop after
      //    ONE fresh mint and never retry a dead URL in a loop.
      {
        tracker.setStep('broken fresh URL settles after single retry');
        deadGeneration = 0;
        serveBrokenFresh = false;
        const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
        await context.addInitScript((origin) => {
          window.__TEST_MINTS__ = [];
          window.__TEST_GEN__ = 1;
          window.__LEGION_MOCK_OVERRIDE_CONTENT_URL__ = async ({ path: p }) => {
            if (String(p || '').endsWith('.mp4')) {
              const gen = window.__TEST_GEN__++;
              const url = `${origin}/test-media/${gen}/clip.webm`;
              window.__TEST_MINTS__.push(url);
              return { url, expires_in: 120 };
            }
            return null;
          };
        }, baseUrl);

        const page = tracker.attach(await context.newPage(), {
          label: 'teardown-broken-fresh',
          collectPage: pageErrors,
        });

        await page.goto(`${baseUrl}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
        await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'gen 1 video[src] (part 2)');
        await tracker.waitForFn(
          page,
          () => {
            const v = document.querySelector('.viewer video');
            return Boolean(v && v.readyState >= 1);
          },
          null,
          'gen 1 readyState >= 1 (part 2)',
        );

        await page.evaluate(() => window.__LEGION_MOCK_TRIGGER_WINDOW_HIDDEN__?.());
        deadGeneration = 1;
        serveBrokenFresh = true;
        await page.evaluate(() => {
          const v = document.querySelector('.viewer video');
          v.dispatchEvent(new Event('error'));
        });
        await waitForLayoutSettled(page, 4);
        assert.equal(await page.evaluate(() => window.__TEST_MINTS__.length), 1);

        // Press Play -> mints exactly ONE fresh URL (gen 2), which returns 410 -> settles on honest
        // error screen without ever minting gen 3 or looping.
        await page.keyboard.press('Space');
        await tracker.waitForLocator(page.locator('.viewer-stage .said'), 'honest error screen after broken gen 2');
        await waitForLayoutSettled(page, 4);
        assert.equal(await page.evaluate(() => window.__TEST_MINTS__.length), 2);
        await context.close();
      }

      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    } finally {
      await browser.close();
      await server.stop();
    }
  },
);

browserTest(
  'Player: wide 1440x900 two-pane split layout isolates inert panes and rail, restores opener focus in either pane, and handles full screen and window hide',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'wide-1440-split',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('.split-left .tile', { hasText: 'Passport.pdf' }).waitFor({ timeout: 5000 });
    await page.locator('.split-right .grid a.cell[aria-label="Arrival.mp4"]').waitFor({ timeout: 5000 });

    // Confirm two-pane wide shell structure at 1440x900 (two panes + nav rail, no narrow page header).
    const shellCheck = await page.evaluate(() => ({
      hasLeft: Boolean(document.querySelector('.split-left')),
      hasRight: Boolean(document.querySelector('.split-right')),
      hasNav: Boolean(document.querySelector('nav.nav')),
      hasNarrowHeader: Boolean(document.querySelector('header.top')),
    }));
    assert.deepEqual(shellCheck, { hasLeft: true, hasRight: true, hasNav: true, hasNarrowHeader: false });

    const assertShellInert = async (expectedInert) => {
      const state = await page.evaluate(() => ({
        left: Boolean(document.querySelector('.split-left')?.closest('[inert]')),
        right: Boolean(document.querySelector('.split-right')?.closest('[inert]')),
        nav: Boolean(document.querySelector('nav.nav')?.closest('[inert]')),
      }));
      assert.deepEqual(state, { left: expectedInert, right: expectedInert, nav: expectedInert });
    };

    // 1. Open viewer from a left-pane pinned file tile -> covers full 1440x900 viewport, marks
    //    both panes and the navigation rail inert, traps Tab inside viewer, and restores focus to
    //    that exact left-pane tile on close.
    const leftTile = page.locator('.split-left .tile', { hasText: 'Passport.pdf' });
    await leftTile.focus();
    await leftTile.click();
    await page.locator('.viewer').waitFor({ timeout: 5000 });
    await assertShellInert(true);

    const viewerBox = await page.locator('.viewer').boundingBox();
    assert.ok(
      viewerBox &&
        viewerBox.x === 0 &&
        viewerBox.y === 0 &&
        viewerBox.width === 1440 &&
        viewerBox.height === 900,
      `Expected viewer to cover full 1440x900 viewport, got ${JSON.stringify(viewerBox)}`,
    );

    // Tab cycling stays strictly inside `.viewer`.
    for (let i = 0; i < 6; i += 1) {
      await page.keyboard.press('Tab');
      const inViewer = await page.evaluate(() => Boolean(document.querySelector('.viewer')?.contains(document.activeElement)));
      assert.equal(inViewer, true, `Tab step ${i + 1} must stay inside .viewer`);
    }

    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'detached', timeout: 5000 });
    await assertShellInert(false);
    const leftFocusAfterClose = await page.evaluate(() => ({
      inLeft: Boolean(document.activeElement?.closest('.split-left')),
      text: document.activeElement?.textContent?.trim() || '',
    }));
    assert.equal(leftFocusAfterClose.inLeft, true, 'Focus must return inside .split-left');
    assert.ok(
      leftFocusAfterClose.text.includes('Passport.pdf'),
      `Expected focus on Passport.pdf tile, got "${leftFocusAfterClose.text}"`,
    );

    // 2. Open video from the right pane -> both panes and rail inert, Full screen enter/exit works,
    //    window-hidden pauses playback in place & leaves full screen, and close returns focus to
    //    the right-pane Arrival.mp4 cell.
    const rightCell = page.locator('.split-right .grid a.cell[aria-label="Arrival.mp4"]');
    await rightCell.focus();
    await rightCell.click();
    await page.locator('.viewer video.viewer-video[src]').waitFor({ timeout: 5000 });
    await tracker.waitForFn(
      page,
      () => {
        const v = document.querySelector('.viewer video');
        return Boolean(v && v.readyState >= 1);
      },
      null,
      'wide split right-pane video readyState >= 1',
    );
    await assertShellInert(true);

    await page.evaluate(() => {
      window.__LEGION_MOCK_IMMERSIVE_CALLS__.length = 0;
      window.__LEGION_MOCK_KEEP_SCREEN_ON_CALLS__.length = 0;
    });
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();
    assert.deepEqual(
      await page.evaluate(() => window.__LEGION_MOCK_IMMERSIVE_CALLS__.map((c) => c.on)),
      [true],
    );
    await page.locator('.viewer button[aria-label="Exit full screen"]').click();
    await page.locator('.viewer button[aria-label="Full screen"]').waitFor();
    assert.deepEqual(
      await page.evaluate(() => window.__LEGION_MOCK_IMMERSIVE_CALLS__.map((c) => c.on)),
      [true, false],
    );

    // Re-enter full screen, start playing, then trigger desktop window-hidden -> pauses in place,
    // leaves full screen, releases keepScreenOn, and keeps the viewer open.
    await page.locator('.viewer button[aria-label="Full screen"]').click();
    await page.locator('.viewer button[aria-label="Exit full screen"]').waitFor();
    await page.evaluate(async () => {
      const v = document.querySelector('.viewer video');
      await v.play();
    });
    await tracker.waitForFn(
      page,
      () => {
        const v = document.querySelector('.viewer video');
        return Boolean(v && !v.paused);
      },
      null,
      'wide split video playing before window hide',
    );

    await page.evaluate(() => window.__LEGION_MOCK_TRIGGER_WINDOW_HIDDEN__?.());
    await page.locator('.viewer button[aria-label="Full screen"]').waitFor();
    const afterHideVideo = await page.evaluate(() => ({
      paused: document.querySelector('.viewer video')?.paused ?? false,
      keepScreenOnLast: window.__LEGION_MOCK_KEEP_SCREEN_ON_CALLS__.at(-1)?.on,
      exitFsButtons: document.querySelectorAll('.viewer button[aria-label="Exit full screen"]').length,
      viewerStillOpen: Boolean(document.querySelector('.viewer')),
    }));
    assert.equal(afterHideVideo.paused, true);
    assert.equal(afterHideVideo.keepScreenOnLast, false);
    assert.equal(afterHideVideo.exitFsButtons, 0);
    assert.equal(afterHideVideo.viewerStillOpen, true);

    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'detached' });
    await assertShellInert(false);
    const rightFocusAfterClose = await page.evaluate(() => ({
      inRight: Boolean(document.activeElement?.closest('.split-right')),
      label: document.activeElement?.getAttribute?.('aria-label') || '',
    }));
    assert.equal(rightFocusAfterClose.inRight, true, 'Focus must return inside .split-right');
    assert.equal(rightFocusAfterClose.label, 'Arrival.mp4');

    // 3. Open music row from the right pane -> both panes and rail inert, window-hidden pauses
    //    audio in place, and close returns focus to that exact right-pane music row.
    tracker.setStep('wide split right-pane music playback and window hide');
    await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
      waitUntil: 'domcontentloaded',
    });
    const musicRow = page.locator('.split-right a.row', { hasText: '01 - Morning Light.flac' });
    await musicRow.waitFor();
    await musicRow.focus();
    await musicRow.click();
    await tracker.waitForLocator(page.locator('.viewer audio[src]'), 'wide split music audio[src]', {
      state: 'attached',
    });
    await assertShellInert(true);

    await page.evaluate(async () => {
      const a = document.querySelector('.viewer audio');
      await a.play();
    });
    await tracker.waitForFn(
      page,
      () => {
        const a = document.querySelector('.viewer audio');
        return Boolean(a && !a.paused);
      },
      null,
      'wide split music playing before window hide',
    );
    await page.evaluate(() => window.__LEGION_MOCK_TRIGGER_WINDOW_HIDDEN__?.());
    assert.equal(await page.evaluate(() => document.querySelector('.viewer audio')?.paused), true);

    await page.keyboard.press('Escape');
    await page.locator('.viewer').waitFor({ state: 'detached' });
    await assertShellInert(false);
    const musicFocusAfterClose = await page.evaluate(() => ({
      inRight: Boolean(document.activeElement?.closest('.split-right')),
      text: document.activeElement?.textContent?.trim() || '',
    }));
    assert.equal(musicFocusAfterClose.inRight, true, 'Focus must return inside .split-right for music row');
    assert.ok(
      musicFocusAfterClose.text.includes('01 - Morning Light.flac'),
      `Expected focus on 01 - Morning Light.flac row, got "${musicFocusAfterClose.text}"`,
    );

    assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
    assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
    await server.stop();
  }
});

const PLAYER_PNG_CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i += 1) {
    let c = i;
    for (let k = 0; k < 8; k += 1) {
      c = (c & 1) !== 0 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[i] = c >>> 0;
  }
  return table;
})();

function makePlayerSolidPng(width, height, r, g, b) {
  function crc32(buf) {
    let c = 0xffffffff;
    for (let i = 0; i < buf.length; i += 1) {
      c = PLAYER_PNG_CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
    }
    return (c ^ 0xffffffff) >>> 0;
  }
  function chunk(type, data) {
    const typeBuf = Buffer.from(type, 'ascii');
    const lenBuf = Buffer.alloc(4);
    lenBuf.writeUInt32BE(data.length, 0);
    const crcBuf = Buffer.alloc(4);
    crcBuf.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
    return Buffer.concat([lenBuf, typeBuf, data, crcBuf]);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 2;
  const stride = 1 + width * 3;
  const raw = Buffer.alloc(height * stride);
  for (let y = 0; y < height; y += 1) {
    const row = y * stride;
    raw[row] = 0;
    for (let x = 0; x < width; x += 1) {
      const px = row + 1 + x * 3;
      raw[px] = r;
      raw[px + 1] = g;
      raw[px + 2] = b;
    }
  }
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function makePlayerWav({ title = null, artist = null, album = null, coverPng = null, durationSecs = 2 } = {}) {
  const sampleRate = 8000;
  const byteRate = sampleRate * 2;
  const dataLen = byteRate * durationSecs;

  const fmt = Buffer.alloc(24);
  fmt.write('fmt ', 0, 'ascii');
  fmt.writeUInt32LE(16, 4);
  fmt.writeUInt16LE(1, 8);
  fmt.writeUInt16LE(1, 10);
  fmt.writeUInt32LE(sampleRate, 12);
  fmt.writeUInt32LE(byteRate, 16);
  fmt.writeUInt16LE(2, 20);
  fmt.writeUInt16LE(16, 22);

  const dataHdr = Buffer.alloc(8);
  dataHdr.write('data', 0, 'ascii');
  dataHdr.writeUInt32LE(dataLen, 4);
  const parts = [Buffer.from('WAVE', 'ascii'), fmt, dataHdr, Buffer.alloc(dataLen)];

  const frames = [];
  function textFrame(id, value) {
    const payload = Buffer.concat([Buffer.from([3]), Buffer.from(value, 'utf8')]);
    const hdr = Buffer.alloc(10);
    hdr.write(id, 0, 'ascii');
    hdr.writeUInt32BE(payload.length, 4);
    frames.push(hdr, payload);
  }
  if (title) textFrame('TIT2', title);
  if (artist) textFrame('TPE1', artist);
  if (album) textFrame('TALB', album);
  if (coverPng) {
    const payload = Buffer.concat([
      Buffer.from([0]),
      Buffer.from('image/png', 'ascii'),
      Buffer.from([0, 3]),
      Buffer.from('cover', 'ascii'),
      Buffer.from([0]),
      coverPng,
    ]);
    const hdr = Buffer.alloc(10);
    hdr.write('APIC', 0, 'ascii');
    hdr.writeUInt32BE(payload.length, 4);
    frames.push(hdr, payload);
  }
  if (frames.length > 0) {
    const body = Buffer.concat(frames);
    const len = body.length;
    const id3Hdr = Buffer.from([
      0x49,
      0x44,
      0x33,
      3,
      0,
      0,
      (len >> 21) & 0x7f,
      (len >> 14) & 0x7f,
      (len >> 7) & 0x7f,
      len & 0x7f,
    ]);
    const id3 = Buffer.concat([id3Hdr, body]);
    const chunkHdr = Buffer.alloc(8);
    chunkHdr.write('ID3 ', 0, 'ascii');
    chunkHdr.writeUInt32LE(id3.length, 4);
    parts.push(chunkHdr, id3);
    if (id3.length % 2 === 1) parts.push(Buffer.from([0]));
  }

  const waveBody = Buffer.concat(parts);
  const riff = Buffer.alloc(8);
  riff.write('RIFF', 0, 'ascii');
  riff.writeUInt32LE(waveBody.length, 4);
  return Buffer.concat([riff, waveBody]);
}

browserTest(
  'Player: real legion serve music screen reads title, artist, album, and cover from real fs.tags and fs.thumb',
  { timeout: 120_000 },
  async (t, tracker) => {
    if (!fs.existsSync(LEGION_BIN)) {
      t.skip(`Release binary not found at ${LEGION_BIN}`);
      return;
    }
    const cacheBase = path.join(os.homedir(), '.cache', 'legion-ui-test');
    fs.mkdirSync(cacheBase, { recursive: true });
    const root = fs.mkdtempSync(path.join(cacheBase, 'player-music-real-'));
    const legionHome = path.join(root, 'legion');
    const userHome = path.join(root, 'home');
    const musicDir = path.join(userHome, 'Music');
    const folderCoverDir = path.join(userHome, 'FolderCover');
    fs.mkdirSync(path.join(legionHome, 'config'), { recursive: true });
    fs.mkdirSync(path.join(legionHome, 'state'), { recursive: true });
    fs.mkdirSync(musicDir, { recursive: true });
    fs.mkdirSync(folderCoverDir, { recursive: true });

    const taggedPath = path.join(musicDir, '01 - harbor_file.wav');
    const untaggedPath = path.join(musicDir, '02 - plain_recording.wav');
    const folderTrackPath = path.join(folderCoverDir, '01 - dusk_demo.wav');

    fs.writeFileSync(
      taggedPath,
      makePlayerWav({
        title: 'Harbor Fog',
        artist: 'Maren',
        album: 'North Coast',
        coverPng: makePlayerSolidPng(180, 180, 42, 130, 195),
        durationSecs: 2,
      }),
    );
    fs.writeFileSync(untaggedPath, makePlayerWav({ durationSecs: 2 }));
    fs.writeFileSync(folderTrackPath, makePlayerWav({ durationSecs: 2 }));
    fs.writeFileSync(path.join(folderCoverDir, 'cover.png'), makePlayerSolidPng(160, 160, 210, 120, 45));

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

    const server = await startLegionServer(legionHome, userHome);
    tracker.setServerStderr(server.getStderr);
    const url = server.url;

    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];
    const toolResponses = [];

    try {
      const context = await browser.newContext({ viewport: { width: 1100, height: 720 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'real-music',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });
      page.on('response', async (resp) => {
        if (!resp.url().includes('/v1/call') || resp.request().method() !== 'POST') return;
        try {
          const reqBody = JSON.parse(resp.request().postData() || '{}');
          if (reqBody.tool === 'fs.tags' || reqBody.tool === 'fs.thumb') {
            const json = await resp.json();
            toolResponses.push({ tool: reqBody.tool, args: reqBody.args, status: resp.status(), body: json });
          }
        } catch {
          // Ignore aborted or non-JSON responses during teardown.
        }
      });

      tracker.setStep('real music folder listing embedded cover');
      await page.goto(`${url}/#token=${encodeURIComponent(tokenOut.token)}`, { waitUntil: 'domcontentloaded' });
      await page.locator('.nav a[href="#/files"]').waitFor();

      // 1. Folder listing renders the real embedded cover via real `fs.thumb` (`size: 128`).
      await page.goto(`${url}/#/files/tower/${encodeURIComponent(musicDir)}`, { waitUntil: 'domcontentloaded' });
      const taggedRow = page.locator('a.row', { hasText: '01 - harbor_file.wav' });
      await taggedRow.waitFor();
      await taggedRow.locator('.thumb img').waitFor();
      await tracker.waitForFn(
        page,
        () => {
          const rows = [...document.querySelectorAll('a.row')];
          const row = rows.find((r) => r.textContent?.includes('01 - harbor_file.wav'));
          const img = row?.querySelector('.thumb img');
          return Boolean(img && img.complete && img.naturalWidth > 0 && img.naturalHeight > 0);
        },
        null,
        'harbor_file.wav folder listing cover decoded',
      );

      // 2. Open tagged track -> title, artist, album, and high-res cover come from real `fs.tags` + `fs.thumb`.
      tracker.setStep('open real tagged track Harbor Fog');
      const taggedTagsRespPromise = tracker.waitForResponse(
        page,
        async (resp) => {
          if (!resp.url().includes('/v1/call') || resp.request().method() !== 'POST') return false;
          try {
            const body = JSON.parse(resp.request().postData() || '{}');
            return body.tool === 'fs.tags' && Array.isArray(body.args?.paths) && body.args.paths.includes(taggedPath);
          } catch {
            return false;
          }
        },
        'real fs.tags for harbor_file.wav',
      );
      const taggedHighResRespPromise = tracker.waitForResponse(
        page,
        async (resp) => {
          if (!resp.url().includes('/v1/call') || resp.request().method() !== 'POST') return false;
          try {
            const body = JSON.parse(resp.request().postData() || '{}');
            return (
              body.tool === 'fs.thumb' &&
              body.args?.size === 1024 &&
              Array.isArray(body.args?.paths) &&
              body.args.paths.includes(taggedPath)
            );
          } catch {
            return false;
          }
        },
        'real fs.thumb (1024) for harbor_file.wav',
      );
      await taggedRow.click();
      await page.locator('.music-screen').waitFor();
      await page.locator('.music-title', { hasText: 'Harbor Fog' }).waitFor();
      await page.locator('.music-subtitle', { hasText: 'Maren · North Coast' }).waitFor();
      const taggedTagsResp = await taggedTagsRespPromise;
      const taggedHighResResp = await taggedHighResRespPromise;
      assert.equal(taggedTagsResp.status(), 200);
      const taggedTagsJson = await taggedTagsResp.json();
      const taggedResult = taggedTagsJson?.results?.find((item) => item.path === taggedPath)?.ok;
      assert.equal(taggedResult?.title, 'Harbor Fog');
      assert.equal(taggedResult?.artist, 'Maren');
      assert.equal(taggedResult?.album, 'North Coast');
      assert.equal(taggedResult?.cover, true);

      assert.equal(taggedHighResResp.status(), 200);
      const taggedHighResJson = await taggedHighResResp.json();
      const highResOk = taggedHighResJson?.results?.find((item) => item.path === taggedPath)?.ok;
      assert.ok(highResOk?.mime?.startsWith('image/') && highResOk?.content, 'Expected decoded image payload from real fs.thumb');
      const expectedCoverSrc = `data:${highResOk.mime};base64,${highResOk.content}`;
      await tracker.waitForFn(
        page,
        (expected) => {
          const img = document.querySelector('img.music-cover');
          return Boolean(img && img.getAttribute('src') === expected && img.complete && img.naturalWidth > 0 && img.naturalHeight > 0);
        },
        expectedCoverSrc,
        'Harbor Fog high-res cover image decoded',
      );
      const taggedDom = await page.evaluate(() => ({
        coverSrc: document.querySelector('img.music-cover')?.getAttribute('src') || '',
        backdropSrc: document.querySelector('.music-backdrop img')?.getAttribute('src') || '',
        mediaSessionTitle: navigator.mediaSession?.metadata?.title || '',
        mediaSessionArtist: navigator.mediaSession?.metadata?.artist || '',
        mediaSessionAlbum: navigator.mediaSession?.metadata?.album || '',
      }));
      assert.equal(taggedDom.coverSrc, expectedCoverSrc);
      assert.equal(taggedDom.backdropSrc, expectedCoverSrc);
      assert.equal(taggedDom.mediaSessionTitle, 'Harbor Fog');
      assert.equal(taggedDom.mediaSessionArtist, 'Maren');
      assert.equal(taggedDom.mediaSessionAlbum, 'North Coast');

      // Real WAV audio stream plays from `/v1/content/`.
      tracker.setStep('play real WAV from /v1/content/');
      await tracker.waitForFn(
        page,
        () => {
          const a = document.querySelector('.viewer audio');
          return Boolean(a && a.readyState >= 1 && Number.isFinite(a.duration) && a.duration > 0);
        },
        null,
        'real WAV audio metadata',
      );
      await page.locator('.music-play-btn').click();
      await tracker.waitForFn(
        page,
        () => {
          const a = document.querySelector('.viewer audio');
          return Boolean(a && !a.paused);
        },
        null,
        'real WAV audio playing',
      );
      await page.locator('.music-play-btn').click();

      // 3. Advance to untagged track -> real `fs.tags` reports `cover: false`, filename shown without extension,
      //    no subtitle, no "Unknown artist", placeholder tile, and no `fs.thumb` (size 1024) call.
      tracker.setStep('advance to untagged track plain_recording.wav');
      const untaggedTagsRespPromise = tracker.waitForResponse(
        page,
        async (resp) => {
          if (!resp.url().includes('/v1/call') || resp.request().method() !== 'POST') return false;
          try {
            const body = JSON.parse(resp.request().postData() || '{}');
            return body.tool === 'fs.tags' && Array.isArray(body.args?.paths) && body.args.paths.includes(untaggedPath);
          } catch {
            return false;
          }
        },
        'real fs.tags for plain_recording.wav',
      );
      await page.locator('.music-screen button[aria-label="Next track"]').click();
      await page.locator('.music-title', { hasText: '02 - plain_recording' }).waitFor();
      const untaggedTagsResp = await untaggedTagsRespPromise;
      await tracker.waitForFn(
        page,
        () => Boolean(document.querySelector('.music-cover-tile')) && !document.querySelector('img.music-cover'),
        null,
        'untagged track placeholder tile',
      );
      assert.equal(await page.locator('.music-subtitle').count(), 0);
      assert.equal(await page.locator('.music-backdrop').count(), 0);
      assert.equal((await page.locator('.viewer').innerText()).includes('Unknown artist'), false);

      const untaggedTagsJson = await untaggedTagsResp.json();
      const untaggedOk = untaggedTagsJson?.results?.find((item) => item.path === untaggedPath)?.ok;
      assert.equal(untaggedOk?.cover, false);
      assert.equal(untaggedOk?.title, undefined);
      const untaggedHighResCall = toolResponses.find(
        (r) =>
          r.tool === 'fs.thumb' &&
          r.args?.size === 1024 &&
          Array.isArray(r.args?.paths) &&
          r.args.paths.includes(untaggedPath),
      );
      assert.equal(untaggedHighResCall, undefined, 'Should skip high-res fs.thumb when real fs.tags reports cover: false');

      // 4. Folder-cover fallback: untagged track next to `cover.png` gets `cover: true` from real `fs.tags`
      //    and loads the decoded folder cover from real `fs.thumb`.
      tracker.setStep('folder-cover fallback for dusk_demo.wav');
      await page.goto(`${url}/#/files/tower/${encodeURIComponent(folderCoverDir)}`, { waitUntil: 'domcontentloaded' });
      const folderRow = page.locator('a.row', { hasText: '01 - dusk_demo.wav' });
      await folderRow.waitFor();
      const folderTagsRespPromise = tracker.waitForResponse(
        page,
        async (resp) => {
          if (!resp.url().includes('/v1/call') || resp.request().method() !== 'POST') return false;
          try {
            const body = JSON.parse(resp.request().postData() || '{}');
            return body.tool === 'fs.tags' && Array.isArray(body.args?.paths) && body.args.paths.includes(folderTrackPath);
          } catch {
            return false;
          }
        },
        'real fs.tags for dusk_demo.wav',
      );
      const folderHighResRespPromise = tracker.waitForResponse(
        page,
        async (resp) => {
          if (!resp.url().includes('/v1/call') || resp.request().method() !== 'POST') return false;
          try {
            const body = JSON.parse(resp.request().postData() || '{}');
            return (
              body.tool === 'fs.thumb' &&
              body.args?.size === 1024 &&
              Array.isArray(body.args?.paths) &&
              body.args.paths.includes(folderTrackPath)
            );
          } catch {
            return false;
          }
        },
        'real fs.thumb (1024) for dusk_demo.wav',
      );
      await folderRow.click();
      await page.locator('.music-title', { hasText: '01 - dusk_demo' }).waitFor();
      const folderTagsResp = await folderTagsRespPromise;
      const folderTagsJson = await folderTagsResp.json();
      assert.equal(folderTagsJson?.results?.find((item) => item.path === folderTrackPath)?.ok?.cover, true);
      const folderHighResResp = await folderHighResRespPromise;
      assert.equal(folderHighResResp.status(), 200);
      const folderCoverJson = await folderHighResResp.json();
      const folderCoverOk = folderCoverJson?.results?.find((item) => item.path === folderTrackPath)?.ok;
      assert.ok(folderCoverOk?.mime?.startsWith('image/') && folderCoverOk?.content);
      const expectedFolderCoverSrc = `data:${folderCoverOk.mime};base64,${folderCoverOk.content}`;
      await tracker.waitForFn(
        page,
        (expected) => {
          const img = document.querySelector('img.music-cover');
          return Boolean(img && img.getAttribute('src') === expected && img.complete && img.naturalWidth > 0 && img.naturalHeight > 0);
        },
        expectedFolderCoverSrc,
        'dusk_demo.wav folder cover image decoded',
      );

      assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
      await context.close();
    } finally {
      await browser.close();
      await server.stop();
      fs.rmSync(root, { recursive: true, force: true });
    }
  },
);
