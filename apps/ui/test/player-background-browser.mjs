// Browser tests for background music persistence, the 56 px small player bar, single-state
// handoff, zero-overlap geometry (phone + desktop + selection bar), swipe-down and 10-minute
// paused auto-dismiss, video pausing music, and shuffle/repeat controls & persistence.

import assert from 'node:assert/strict';

import {
  browserTest,
  launchBrowser,
  startStaticUiServer,
  waitForLayoutSettled,
  waitForMediaReady,
} from './browser-helpers.mjs';

async function swipeDownMiniPlayer(page, tracker) {
  tracker.setStep('swipe down small player bar');
  await tracker.waitForLocator(page.locator('.mini-player'), 'visible .mini-player before swipe down');
  const box = await page.locator('.mini-player').boundingBox();
  assert.ok(box, 'expected visible .mini-player before swipe down');
  const cx = box.x + box.width * 0.4;
  await page.mouse.move(cx, box.y + 12);
  await page.mouse.down();
  await page.mouse.move(cx, box.y + 60, { steps: 6 });
  await page.mouse.up();
}

browserTest(
  'Music background: small player bar geometry, visibility, single state, swipe/10m dismiss, video pause',
  { timeout: 120_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      // 1. Phone viewport (390x844): small player geometry, zero overlap with dock/selection/nav,
      //    position line movement, single state across song screen <-> mini player <-> other tabs,
      //    hidden during full-screen picture and video, and video starting pauses music.
      {
        tracker.setStep('phone small player geometry, single state, and swipe dismiss');
        const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: 'phone-bg-music',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).click();
        await tracker.waitForLocator(page.locator('.music-screen'), 'music screen opened');
        await tracker.waitForLocator(
          page.locator('audio[data-legion-music-audio][src]'),
          'Morning Light audio[src]',
          { state: 'attached' },
        );
        await waitForMediaReady(page.locator('audio[data-legion-music-audio][src]'), 'Morning Light metadata ready', {
          tracker,
          minReadyState: 1,
        });
        // Small player is ABSENT while song screen is showing.
        assert.equal(await page.locator('.mini-player').count(), 0);

        // Start playing and advance playback position slightly.
        await page.locator('.music-play-btn').click();
        await tracker.waitForFn(
          page,
          () => {
            const a = document.querySelector('audio[data-legion-music-audio]');
            return Boolean(a && !a.paused && Number.isFinite(a.duration) && a.duration > 0);
          },
          null,
          'Morning Light playing',
        );
        const firstUrl = await page.evaluate(
          () => document.querySelector('audio[data-legion-music-audio]')?.getAttribute('src') || '',
        );
        await page.evaluate(() => {
          const a = document.querySelector('audio[data-legion-music-audio]');
          a.currentTime = Math.min(a.duration * 0.25, 0.25);
          a.dispatchEvent(new Event('timeupdate'));
        });

        // Leave the song's screen -> music keeps playing and 56 px small player appears above nav.
        await page.locator('.music-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'music screen closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('.mini-player'), 'phone small player bar');
        await waitForLayoutSettled(page, 2);

        const phoneMini = await page.evaluate(() => {
          const mini = document.querySelector('.mini-player');
          const nav = document.querySelector('nav.nav');
          const dock = document.querySelector('.dock');
          const cover = document.querySelector('.mini-player-cover');
          const progress = document.querySelector('.mini-player-progress');
          const fill = document.querySelector('.mini-player-progress i');
          const audio = document.querySelector('audio[data-legion-music-audio]');
          const miniRect = mini.getBoundingClientRect();
          const navRect = nav.getBoundingClientRect();
          const dockRect = dock ? dock.getBoundingClientRect() : null;
          const coverRect = cover.getBoundingClientRect();
          const progRect = progress.getBoundingClientRect();
          return {
            height: miniRect.height,
            miniBottom: miniRect.bottom,
            navTop: navRect.top,
            dockBottom: dockRect ? dockRect.bottom : null,
            miniTop: miniRect.top,
            coverW: coverRect.width,
            coverH: coverRect.height,
            coverRadius: getComputedStyle(cover).borderRadius,
            progressH: progRect.height,
            progressTransform: fill.style.transform,
            title: document.querySelector('.mini-player-title')?.textContent?.trim() || '',
            artist: document.querySelector('.mini-player-artist')?.textContent?.trim() || '',
            playLabel: document.querySelector('.mini-player-actions button')?.getAttribute('aria-label') || '',
            hasNext: Boolean(document.querySelector('.mini-player-actions button[aria-label="Next track"]')),
            audioPaused: audio?.paused,
            overflow: document.documentElement.scrollWidth - window.innerWidth,
          };
        });
        assert.ok(Math.abs(phoneMini.height - 56) <= 1, `Mini player height must be 56px, got ${phoneMini.height}`);
        assert.ok(
          Math.abs(phoneMini.miniBottom - phoneMini.navTop) <= 1.5,
          `Mini player must sit directly above tab bar: bottom=${phoneMini.miniBottom} vs navTop=${phoneMini.navTop}`,
        );
        if (phoneMini.dockBottom != null) {
          assert.ok(
            phoneMini.dockBottom <= phoneMini.miniTop + 1,
            `Dock must sit cleanly above mini player: dockBottom=${phoneMini.dockBottom} vs miniTop=${phoneMini.miniTop}`,
          );
        }
        assert.ok(Math.abs(phoneMini.coverW - 40) <= 1 && Math.abs(phoneMini.coverH - 40) <= 1);
        assert.equal(phoneMini.coverRadius, '8px');
        assert.ok(Math.abs(phoneMini.progressH - 2) <= 0.5);
        assert.notEqual(phoneMini.progressTransform, 'scaleX(0)', '2 px position line should reflect progress');
        assert.equal(phoneMini.title, 'Morning Light');
        assert.equal(phoneMini.artist, 'Klara');
        assert.equal(phoneMini.playLabel, 'Pause');
        assert.equal(phoneMini.hasNext, true);
        assert.equal(phoneMini.audioPaused, false);
        assert.ok(phoneMini.overflow <= 1, `Horizontal overflow with mini player: ${phoneMini.overflow}px`);

        // Verify position line advances when playback moves forward.
        const beforeScale = phoneMini.progressTransform;
        await page.evaluate(() => {
          const a = document.querySelector('audio[data-legion-music-audio]');
          a.currentTime = Math.min(a.duration * 0.5, 0.45);
          a.dispatchEvent(new Event('timeupdate'));
        });
        await tracker.waitForFn(
          page,
          (prev) => (document.querySelector('.mini-player-progress i')?.style.transform || '') !== prev,
          beforeScale,
          'small player position line advances',
        );

        // Enter selection mode (FilesDock .bar4) and verify ZERO overlap with mini player and last list row.
        tracker.setStep('selection bar zero overlap with small player');
        await page.locator('.dock button[aria-label="More"]').click();
        await page.locator('.sheet button .title', { hasText: /^Select$/ }).click();
        await tracker.waitForLocator(page.locator('.dock .bar4'), 'selection action bar');
        await waitForLayoutSettled(page, 2);
        const selectionGeom = await page.evaluate(() => {
          const main = document.querySelector('.main');
          main.scrollTop = main.scrollHeight;
          const rows = [...document.querySelectorAll('.main .list a.row, .main .list button.row, .main .list .row')];
          const lastRow = rows[rows.length - 1];
          const bar4 = document.querySelector('.dock .bar4').getBoundingClientRect();
          const mini = document.querySelector('.mini-player').getBoundingClientRect();
          const navEl = document.querySelector('nav.nav');
          const nav = navEl ? navEl.getBoundingClientRect() : null;
          const lastRect = lastRow.getBoundingClientRect();
          return {
            lastRowBottom: lastRect.bottom,
            bar4Top: bar4.top,
            bar4Bottom: bar4.bottom,
            miniTop: mini.top,
            miniBottom: mini.bottom,
            navTop: nav ? nav.top : window.innerHeight,
            mainPaddingBottom: parseFloat(getComputedStyle(main).paddingBottom),
          };
        });
        assert.ok(
          selectionGeom.lastRowBottom <= selectionGeom.bar4Top + 1,
          `Last row overlapped selection bar: ${selectionGeom.lastRowBottom} > ${selectionGeom.bar4Top}`,
        );
        assert.ok(
          selectionGeom.bar4Bottom <= selectionGeom.miniTop + 1,
          `Selection bar overlapped mini player: ${selectionGeom.bar4Bottom} > ${selectionGeom.miniTop}`,
        );
        assert.ok(
          selectionGeom.miniBottom <= selectionGeom.navTop + 1.5,
          `Mini player overlapped tab bar: ${selectionGeom.miniBottom} > ${selectionGeom.navTop}`,
        );
        assert.ok(
          selectionGeom.mainPaddingBottom >= 56,
          `List container needs >= 56px bottom padding, got ${selectionGeom.mainPaddingBottom}`,
        );
        await page.locator('header.top button[aria-label="Stop selecting"]').click();

        // Pause on small player -> navigate to Machines tab -> small player stays visible ->
        // tap small player -> opens song screen showing paused at the exact same position!
        tracker.setStep('small player single state across tabs and return to song screen');
        await page.locator('.mini-player-actions button[aria-label="Pause"]').click();
        await tracker.waitForLocator(
          page.locator('.mini-player-actions button[aria-label="Play"]'),
          'small player paused Play button',
        );
        const pausedPos = await page.evaluate(
          () => document.querySelector('audio[data-legion-music-audio]')?.currentTime ?? 0,
        );
        await page.locator('nav.nav a[href="#/machines"]').click();
        await tracker.waitForLocator(page.locator('header.top h1', { hasText: 'Machines' }), 'Machines tab');
        await tracker.waitForLocator(page.locator('.mini-player'), 'small player persists on Machines tab');

        await page.locator('.mini-player-open').click();
        await tracker.waitForLocator(page.locator('.viewer .music-screen'), 'song screen reopened from small player');
        assert.equal(await page.locator('.mini-player').count(), 0);
        assert.equal(await page.locator('.music-play-btn').getAttribute('aria-label'), 'Play');
        const returnedPos = await page.evaluate(
          () => document.querySelector('audio[data-legion-music-audio]')?.currentTime ?? 0,
        );
        assert.ok(Math.abs(returnedPos - pausedPos) < 0.1, `Position drifted: ${pausedPos} -> ${returnedPos}`);

        // Resume playing on song screen, close back to Machines, then navigate in-app to Holiday folder:
        // opening a full-screen picture hides mini player (music keeps playing), closing picture restores it;
        // starting a video pauses music and hides mini player!
        tracker.setStep('small player hidden on picture and video, video play pauses music');
        await page.evaluate(() => {
          const a = document.querySelector('audio[data-legion-music-audio]');
          if (a) a.currentTime = 0.05;
        });
        await page.locator('.music-play-btn').click();
        await tracker.waitForLocator(
          page.locator('.music-play-btn[aria-label="Pause"]'),
          'song resumed before picture/video check',
        );
        await page.locator('.music-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'music viewer closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('header.top h1', { hasText: 'Machines' }), 'Machines tab restored');
        await tracker.waitForLocator(page.locator('.mini-player'), 'small player restored');

        await page.evaluate(() => {
          window.location.hash = '#/files/tower/%2Fhome%2Fme%2FVideos%2FHoliday';
        });
        await tracker.waitForLocator(page.locator('.grid a.cell[aria-label="IMG_2041.jpg"]'), 'Holiday picture cell');
        await page.locator('.grid a.cell[aria-label="IMG_2041.jpg"]').click();
        await tracker.waitForLocator(page.locator('.viewer img.viewer-img'), 'full-screen picture opened');
        assert.equal(await page.locator('.mini-player').count(), 0, 'Mini player must be absent on full-screen picture');
        await page.locator('.viewer-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'picture viewer closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('.mini-player'), 'small player restored after picture');

        // Open a video and press play -> pauses music, mini player absent during video, reappears paused after close.
        await page.evaluate(() => {
          const a = document.querySelector('audio[data-legion-music-audio]');
          if (a && a.paused) {
            a.currentTime = 0.05;
            a.play().catch(() => {});
          }
        });
        await page.locator('.grid a.cell[aria-label="Arrival.mp4"]').click();
        await tracker.waitForLocator(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 video[src]');
        await waitForMediaReady(page.locator('.viewer video.viewer-video[src]'), 'Arrival.mp4 ready', {
          tracker,
          minReadyState: 1,
        });
        assert.equal(await page.locator('.mini-player').count(), 0, 'Mini player must be absent during video player');
        await page.locator('.viewer .player-center-btn[aria-label="Play"]').click();
        await tracker.waitForFn(
          page,
          () => document.querySelector('audio[data-legion-music-audio]')?.paused === true,
          null,
          'starting video pauses music',
        );
        await page.locator('.player-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'video viewer closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('.mini-player'), 'small player reappears paused after video');
        assert.equal(
          await page.locator('.mini-player-actions button').first().getAttribute('aria-label'),
          'Play',
        );

        // Opening another song replaces the first song and revokes its content URL.
        tracker.setStep('opening second song revokes first URL and applies FileNameSingle');
        await page.evaluate(() => {
          window.location.hash = '#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum';
        });
        await tracker.waitForLocator(
          page.locator('a.row', { hasText: 'field_recording_untagged.wav' }),
          'untagged song row',
        );
        await page.locator('a.row', { hasText: 'field_recording_untagged.wav' }).click();
        await tracker.waitForLocator(
          page.locator('.music-title', { hasText: 'field_recording_untagged' }),
          'untagged song screen',
        );
        await page.locator('.music-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'untagged song screen closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('.mini-player'), 'untagged song small player');

        const afterReplace = await page.evaluate((prevUrl) => ({
          revokedPrev: Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(prevUrl),
          title: document.querySelector('.mini-player-title')?.textContent?.trim() || '',
          hasArtist: Boolean(document.querySelector('.mini-player-artist')),
          hasNext: Boolean(document.querySelector('.mini-player-actions button[aria-label="Next track"]')),
          currentUrl: document.querySelector('audio[data-legion-music-audio]')?.getAttribute('src') || '',
        }), firstUrl);
        assert.equal(afterReplace.revokedPrev, true, 'Opening another song must revoke first song URL');
        assert.equal(afterReplace.title, 'field_recording_untagged');
        assert.equal(afterReplace.hasArtist, false, 'Untagged song must omit artist line (never Unknown artist)');
        assert.equal(afterReplace.hasNext, false, 'Next button must be absent on last song');

        // Small player title uses FileNameSingle (one ellipsis keeping the ending, full accessible name).
        await page.setViewportSize({ width: 250, height: 844 });
        await tracker.waitForFn(
          page,
          () => {
            const slot = document.querySelector('.mini-player-title .fn-slot');
            return Boolean(slot && slot.getAttribute('data-fn-ready') === '1' && slot.textContent?.includes('…'));
          },
          null,
          'narrow small player fitted FileNameSingle title',
        );
        const miniLongTitle = await page.evaluate(() => {
          const wrap = document.querySelector('.mini-player-title');
          const slot = wrap?.querySelector('.fn-slot');
          const lines = [...(slot?.querySelectorAll('.fn-line') || [])];
          const drawn = lines.map((l) => l.textContent || '').join('');
          return {
            ariaLabel: wrap?.getAttribute('aria-label') || '',
            titleAttr: wrap?.getAttribute('title') || '',
            lineCount: lines.length,
            drawn,
            ellipsisCount: (drawn.match(/…/g) || []).length,
            lineOverflows: lines.some((l) => l.scrollWidth > (slot?.clientWidth || 0) + 2),
            overflow: document.documentElement.scrollWidth - window.innerWidth,
          };
        });
        assert.equal(miniLongTitle.ariaLabel, 'field_recording_untagged');
        assert.equal(miniLongTitle.titleAttr, 'field_recording_untagged');
        assert.equal(miniLongTitle.lineCount, 1, `Mini player title must stay 1 line, got ${miniLongTitle.lineCount}`);
        assert.equal(miniLongTitle.ellipsisCount, 1, `Expected single ellipsis, got "${miniLongTitle.drawn}"`);
        assert.ok(
          miniLongTitle.drawn.endsWith('untagged'),
          `Mini player fitted title must keep ending: "${miniLongTitle.drawn}"`,
        );
        assert.equal(miniLongTitle.lineOverflows, false, `Mini player title overflowed slot: "${miniLongTitle.drawn}"`);
        assert.ok(miniLongTitle.overflow <= 1, `Narrow mini player horizontal overflow: ${miniLongTitle.overflow}px`);
        await page.setViewportSize({ width: 390, height: 844 });
        await waitForLayoutSettled(page, 2);

        // Swipe down dismisses small player and revokes active stream URL.
        await swipeDownMiniPlayer(page, tracker);
        await tracker.waitForLocator(page.locator('.mini-player'), 'small player dismissed after swipe', {
          state: 'detached',
        });
        const afterSwipe = await page.evaluate((url) => ({
          revoked: Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(url),
          hasSrc: Boolean(document.querySelector('audio[data-legion-music-audio]')?.hasAttribute('src')),
          msState: navigator.mediaSession?.playbackState || '',
        }), afterReplace.currentUrl);
        assert.equal(afterSwipe.revoked, true, 'Swiping down must revoke stream URL');
        assert.equal(afterSwipe.hasSrc, false);
        assert.equal(afterSwipe.msState, 'none');

        await context.close();
      }

      // 2. Desktop wide viewport (1280x800): small player sits at bottom of main column (right of nav rail)
      //    and never overlaps left rail or bottom row of split panes.
      {
        tracker.setStep('desktop wide viewport small player geometry');
        const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: 'desktop-bg-music',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('.split-right a.row', { hasText: '01 - Morning Light.flac' }).click();
        await tracker.waitForLocator(page.locator('.music-screen'), 'desktop music screen');
        await page.locator('.music-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), 'desktop music viewer closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('.mini-player'), 'desktop small player bar');
        await waitForLayoutSettled(page, 2);

        const desktopGeom = await page.evaluate(() => {
          const rail = document.querySelector('nav.nav').getBoundingClientRect();
          const mini = document.querySelector('.mini-player').getBoundingClientRect();
          const rightBody = document.querySelector('.split-right .split-body');
          rightBody.scrollTop = rightBody.scrollHeight;
          const rows = [...rightBody.querySelectorAll('a.row')];
          const lastRow = rows[rows.length - 1].getBoundingClientRect();
          return {
            railRight: rail.right,
            miniLeft: mini.left,
            miniHeight: mini.height,
            miniTop: mini.top,
            miniBottom: mini.bottom,
            viewportH: window.innerHeight,
            lastRowBottom: lastRow.bottom,
            splitBodyPadBottom: parseFloat(getComputedStyle(rightBody).paddingBottom),
          };
        });
        assert.ok(
          desktopGeom.miniLeft >= desktopGeom.railRight - 1,
          `Desktop mini player must sit in main column right of nav rail: ${desktopGeom.miniLeft} < ${desktopGeom.railRight}`,
        );
        assert.ok(Math.abs(desktopGeom.miniHeight - 56) <= 1);
        assert.ok(Math.abs(desktopGeom.miniBottom - desktopGeom.viewportH) <= 1.5);
        assert.ok(
          desktopGeom.lastRowBottom <= desktopGeom.miniTop + 1,
          `Desktop last row overlapped mini player: ${desktopGeom.lastRowBottom} > ${desktopGeom.miniTop}`,
        );
        assert.ok(desktopGeom.splitBodyPadBottom >= 56);
        await context.close();
      }

      // 3. Controlled-clock 10-minute paused auto-dismiss releases stream and clears small player.
      {
        tracker.setStep('controlled-clock 10-minute paused auto-dismiss');
        const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
        const page = tracker.attach(await context.newPage(), {
          label: '10m-paused-dismiss',
          collectConsole: consoleErrors,
          collectPage: pageErrors,
        });

        await page.addInitScript(() => {
          let now = 5_000_000;
          let nextId = 1;
          const timers = new Map();
          window.__LEGION_TEST_PLAYER_CLOCK__ = {
            now: () => now,
            setTimeout: (fn, ms) => {
              const id = nextId++;
              timers.set(id, { at: now + Math.max(0, Number(ms) || 0), fn });
              return id;
            },
            clearTimeout: (id) => {
              timers.delete(id);
            },
            advance(ms) {
              const target = now + ms;
              while (true) {
                let earliest = null;
                for (const [id, entry] of timers.entries()) {
                  if (entry.at <= target && (!earliest || entry.at < earliest.entry.at)) {
                    earliest = { id, entry };
                  }
                }
                if (!earliest) break;
                timers.delete(earliest.id);
                now = earliest.entry.at;
                earliest.entry.fn();
              }
              now = target;
            },
          };
        });

        await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
          waitUntil: 'domcontentloaded',
        });
        await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).click();
        await tracker.waitForLocator(
          page.locator('audio[data-legion-music-audio][src]'),
          '10m test audio[src]',
          { state: 'attached' },
        );
        const urlBeforeDismiss = await page.evaluate(
          () => document.querySelector('audio[data-legion-music-audio]')?.getAttribute('src') || '',
        );
        await page.locator('.music-top button[aria-label="Close"]').click();
        await tracker.waitForLocator(page.locator('.viewer'), '10m test viewer closed', { state: 'detached' });
        await tracker.waitForLocator(page.locator('.mini-player'), '10m test small player shown');

        // Advance 9 minutes 59 seconds while paused -> small player MUST still be there.
        await page.evaluate(() => window.__LEGION_TEST_PLAYER_CLOCK__.advance(599_000));
        await waitForLayoutSettled(page, 2);
        assert.equal(await page.locator('.mini-player').count(), 1);

        // Advance final 1 second (total 10 minutes paused) -> dismissed & URL revoked!
        await page.evaluate(() => window.__LEGION_TEST_PLAYER_CLOCK__.advance(1_000));
        await tracker.waitForLocator(page.locator('.mini-player'), 'small player auto-dismissed at 10m', {
          state: 'detached',
        });
        const revoked = await page.evaluate(
          (u) => Array.isArray(window.__LEGION_MOCK_REVOKED_URLS__) && window.__LEGION_MOCK_REVOKED_URLS__.includes(u),
          urlBeforeDismiss,
        );
        assert.equal(revoked, true, '10-minute paused auto-dismiss must revoke content URL');
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
  'Music background: shuffle order, repeat-folder wrap, repeat-one, and localStorage persistence',
  { timeout: 90_000 },
  async (_t, tracker) => {
    const server = await startStaticUiServer();
    const browser = await launchBrowser({ autoplay: true });
    const consoleErrors = [];
    const pageErrors = [];

    try {
      const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
      const page = tracker.attach(await context.newPage(), {
        label: 'shuffle-repeat',
        collectConsole: consoleErrors,
        collectPage: pageErrors,
      });

      tracker.setStep('open album track and inspect shuffle/repeat layout');
      await page.goto(`${server.url}/?mock#/files/tower/%2Fhome%2Fme%2FMusic%2FAlbum`, {
        waitUntil: 'domcontentloaded',
      });
      await page.locator('a.row', { hasText: '01 - Morning Light.flac' }).click();
      await tracker.waitForLocator(page.locator('.music-screen'), 'music screen');
      await tracker.waitForLocator(
        page.locator('audio[data-legion-music-audio][src]'),
        'Morning Light audio[src]',
        { state: 'attached' },
      );
      await waitForMediaReady(page.locator('audio[data-legion-music-audio][src]'), 'Morning Light ready', {
        tracker,
        minReadyState: 1,
      });

      // Quiet Shuffle (left) and Repeat (right) toggles flank transport row, >= 44x44, never disabled.
      const layout = await page.evaluate(() => {
        const shuf = document.querySelector('.music-mode-btn[aria-label^="Shuffle"]');
        const prev = document.querySelector('.music-ctl-btn[aria-label*="track"]');
        const play = document.querySelector('.music-play-btn');
        const next = document.querySelector('.music-ctl-btn[aria-label="Next track"]');
        const rep = document.querySelector('.music-mode-btn[data-repeat]');
        const r = (el) => el.getBoundingClientRect();
        return {
          shuf: r(shuf),
          prev: r(prev),
          play: r(play),
          next: r(next),
          rep: r(rep),
          disabledCount: document.querySelectorAll('.music-transport button[disabled]').length,
        };
      });
      assert.equal(layout.disabledCount, 0);
      assert.ok(layout.shuf.width >= 44 && layout.shuf.height >= 44);
      assert.ok(layout.rep.width >= 44 && layout.rep.height >= 44);
      assert.ok(layout.shuf.right <= layout.prev.left + 1);
      assert.ok(layout.next.right <= layout.rep.left + 1);

      // Enable Shuffle -> current song stays first (1 of 4), Next & Prev walk the shuffled order cleanly.
      tracker.setStep('verify shuffle keeps current song first and holds order');
      const shuffleBtn = page.locator('.music-mode-btn[aria-label^="Shuffle"]');
      await shuffleBtn.click();
      assert.equal(await shuffleBtn.getAttribute('aria-pressed'), 'true');
      const title0 = await page.locator('.music-title').innerText();
      assert.equal(title0, 'Morning Light');

      await page.locator('.music-ctl-btn[aria-label="Next track"]').click();
      await tracker.waitForFn(
        page,
        (prevTitle) => document.querySelector('.music-title')?.textContent?.trim() !== prevTitle,
        title0,
        'next shuffled track title',
      );
      const title1 = await page.locator('.music-title').innerText();

      // Previous returns to the exact first track of the shuffled order.
      await page.locator('.music-ctl-btn[aria-label="Previous track"]').click();
      await tracker.waitForLocator(page.locator('.music-title', { hasText: title0 }), 'returned to first shuffled track');
      // Next returns to the exact same second track (order preserved while shuffle stays on!).
      await page.locator('.music-ctl-btn[aria-label="Next track"]').click();
      await tracker.waitForLocator(page.locator('.music-title', { hasText: title1 }), 'same second shuffled track');

      // Turn shuffle off so we can test repeat-folder wrap and repeat-one deterministically.
      await shuffleBtn.click();
      assert.equal(await shuffleBtn.getAttribute('aria-pressed'), 'false');

      // Cycle repeat: off -> folder -> one -> off.
      tracker.setStep('verify repeat-folder wrap and repeat-one replay');
      const repeatBtn = page.locator('.music-mode-btn[data-repeat]');
      assert.equal(await repeatBtn.getAttribute('data-repeat'), 'off');
      await repeatBtn.click();
      assert.equal(await repeatBtn.getAttribute('data-repeat'), 'folder');
      assert.equal(await repeatBtn.getAttribute('aria-pressed'), 'true');

      // Go to last song (`field_recording_untagged.wav`): with repeat=folder, Next IS present and wraps to first song!
      await page.locator('.music-top button[aria-label="Close"]').click();
      await tracker.waitForLocator(page.locator('.viewer'), 'viewer closed before last song', { state: 'detached' });
      await page.locator('a.row', { hasText: 'field_recording_untagged.wav' }).click();
      await tracker.waitForLocator(
        page.locator('.music-title', { hasText: 'field_recording_untagged' }),
        'last song screen',
      );
      await tracker.waitForLocator(
        page.locator('audio[data-legion-music-audio][src]'),
        'last song audio[src]',
        { state: 'attached' },
      );
      assert.equal(await page.locator('.music-ctl-btn[aria-label="Next track"]').count(), 1);
      await page.evaluate(() => {
        document.querySelector('audio[data-legion-music-audio]').dispatchEvent(new Event('ended'));
      });
      await tracker.waitForLocator(
        page.locator('.music-title', { hasText: 'Morning Light' }),
        'repeat-folder wrapped to Morning Light',
      );

      // Switch to repeat=one -> `ended` repeats Morning Light from 0 and continues playing instead of advancing.
      await repeatBtn.click();
      assert.equal(await repeatBtn.getAttribute('data-repeat'), 'one');
      await tracker.waitForLocator(
        page.locator('audio[data-legion-music-audio][src]'),
        'Morning Light audio[src] for repeat-one',
        { state: 'attached' },
      );
      await waitForMediaReady(page.locator('audio[data-legion-music-audio][src]'), 'Morning Light ready for repeat-one', {
        tracker,
        minReadyState: 1,
      });
      await page.evaluate(() => {
        const a = document.querySelector('audio[data-legion-music-audio]');
        a.currentTime = Math.max(0.5, (a.duration || 1) - 0.1);
        a.dispatchEvent(new Event('ended'));
      });
      await tracker.waitForFn(
        page,
        () => {
          const a = document.querySelector('audio[data-legion-music-audio]');
          return Boolean(a && !a.paused && a.currentTime >= 0 && a.currentTime < 0.4);
        },
        null,
        'repeat-one restarted Morning Light from 0 and kept playing',
      );
      assert.equal(await page.locator('.music-title').innerText(), 'Morning Light');

      // Enable shuffle alongside repeat=one and reload page -> both persist from localStorage!
      tracker.setStep('verify shuffle and repeat persist across reload');
      await shuffleBtn.click();
      await page.reload({ waitUntil: 'domcontentloaded' });
      await tracker.waitForLocator(page.locator('.music-screen'), 'music screen after reload');
      assert.equal(await page.locator('.music-mode-btn[aria-label^="Shuffle"]').getAttribute('aria-pressed'), 'true');
      assert.equal(await page.locator('.music-mode-btn[data-repeat]').getAttribute('data-repeat'), 'one');

      assert.deepEqual(consoleErrors, [], `Console errors: ${consoleErrors.join('; ')}`);
      assert.deepEqual(pageErrors, [], `Page errors: ${pageErrors.join('; ')}`);
      await context.close();
    } finally {
      await browser.close();
      await server.stop();
    }
  },
);
