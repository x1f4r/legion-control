// Suite 5: UI / Browser — real `legion serve` serving a 5,000-entry folder in headless Chromium.
// Measures:
//   1. Time from navigating into the 5,000-entry folder to first paint of rows.
//   2. Scrolling all the way to the end of the 5,000 entries: total scroll time, dropped frames
//      (> 33.3 ms) & janky frames (> 50 ms), max frame gap, Long Task count & total/max blocking ms,
//      and final DOM node count.

import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import {
  readProcMetrics,
  runLegionSync,
  startLegionServe,
  writeThrowawayNode,
} from '../lib/proc.mjs';
import { summarizeSamples } from '../lib/stats.mjs';

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

async function loadPlaywright(repoRoot) {
  const pkgPath = path.join(repoRoot, 'apps', 'ui', 'test', 'node_modules', 'playwright-core', 'index.mjs');
  if (!fs.existsSync(pkgPath)) {
    throw new Error(`playwright-core not found at ${pkgPath}`);
  }
  return import(pathToFileURL(pkgPath).href);
}

async function setupUiVariant(scratchRoot, fixtures, v) {
  const root = path.join(scratchRoot, 'nodes-ui', v.id);
  const legionHome = path.join(root, 'legion');
  const userHome = path.join(root, 'user');
  writeThrowawayNode({ legionHome, userHome, name: 'tower' });

  // Symlink or point directly at the 5,000-entry fixture directory.
  try {
    fs.rmSync(path.join(legionHome, 'config', 'tokens.toml'), { force: true });
  } catch {
    // Ignore.
  }
  const tokOut = JSON.parse(
    runLegionSync(v.bin, ['token.add', 'name=ui', 'principal=operator', '--json'], legionHome, userHome),
  );
  const server = await startLegionServe(v.bin, legionHome, userHome);
  return {
    legionHome,
    userHome,
    token: tokOut.token,
    server,
    folderPath: fixtures.ui5k,
    wrappedFolderPath: fixtures.ui5kWrapped,
  };
}

async function measureFolderOpenAndScroll(
  browser,
  env,
  {
    viewport = { width: 1100, height: 720 },
    folderPath = env.folderPath,
    firstPrefix = 'file_00000',
    lastPrefix = 'file_04999',
    expectWrappedTwoLines = false,
  } = {},
) {
  const context = await browser.newContext({
    viewport,
    colorScheme: 'dark',
  });
  const page = await context.newPage();

  const consoleErrors = [];
  const pageErrors = [];
  let fsListCalls = 0;
  page.on('console', (msg) => {
    if (msg.type() === 'error') consoleErrors.push(msg.text());
  });
  page.on('pageerror', (err) => pageErrors.push(String(err)));
  page.on('request', (req) => {
    if (req.method() === 'POST' && req.url().includes('/v1/call')) {
      const body = req.postData() || '';
      if (body.includes('"fs.list"')) fsListCalls += 1;
    }
  });

  try {
    // 1. Authenticate cleanly before timing the folder open.
    await page.goto(`${env.server.url}/#token=${encodeURIComponent(env.token)}`, {
      waitUntil: 'domcontentloaded',
    });
    await page.locator('nav.nav a', { hasText: 'Files' }).waitFor({ timeout: 15_000 });

    // Navigate to Files root first so opening the 5,000-entry folder measures real in-app open.
    await page.locator('nav.nav a', { hasText: 'Files' }).click();
    await page.locator('h1, h2').first().waitFor({ timeout: 10_000 });
    fsListCalls = 0;

    // 2. Measure time from navigating into the 5,000-entry folder to first paint of its rows.
    const folderHash = `#/files/tower/${encodeURI(folderPath)}`;
    const m0 = readProcMetrics(env.server.pid);
    const openT0 = process.hrtime.bigint();
    await page.evaluate((hash) => {
      window.location.hash = hash;
    }, folderHash);

    await page.locator(`.list a.row[aria-label^="${firstPrefix}"]`).first().waitFor({
      state: 'visible',
      timeout: 20_000,
    });
    // Wait two rAF ticks so layout + paint of the initial rows have actually committed.
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
        ),
    );
    const firstPaintMs = Number(process.hrtime.bigint() - openT0) / 1e6;
    const initialRows = await page.locator('.list a.row').count();

    if (expectWrappedTwoLines) {
      await page.waitForFunction(
        () => {
          const slots = [...document.querySelectorAll('.list a.row .fn-slot')];
          const ready = slots.filter((s) => s.getAttribute('data-fn-ready') === '1');
          return ready.length >= 5 && ready.slice(0, 5).every((s) => s.querySelectorAll('.fn-line').length === 2);
        },
        null,
        { timeout: 15_000 },
      );
    }

    // 3. Install Long Task observer + continuous rAF frame-delta tracker, then scroll to the end
    //    of all 5,000 entries.
    await page.evaluate(() => {
      window.__benchPerf = {
        running: true,
        frames: [],
        longTasks: [],
        lastTs: performance.now(),
      };
      try {
        const po = new PerformanceObserver((list) => {
          for (const entry of list.getEntries()) {
            window.__benchPerf.longTasks.push(entry.duration);
          }
        });
        po.observe({ type: 'longtask', buffered: false });
        window.__benchPerf.po = po;
      } catch {
        // PerformanceObserver for longtask not available.
      }
      function tick(now) {
        const st = window.__benchPerf;
        if (!st || !st.running) return;
        st.frames.push(now - st.lastTs);
        st.lastTs = now;
        requestAnimationFrame(tick);
      }
      requestAnimationFrame(tick);
    });

    const scrollT0 = process.hrtime.bigint();
    // Scroll incrementally to the bottom, letting IntersectionObserver load every page until the
    // pager disappears and all 5,000 entries have been traversed (or a safe timeout elapses).
    const scrollResult = await page.evaluate(
      async ({ firstPfx, lastPfx }) => {
        function getFolderList() {
          const lists = [...document.querySelectorAll('.list')];
          return (
            lists.find(
              (el) =>
                el.querySelector(`a.row[aria-label^="${firstPfx}"]`) ||
                el.querySelector(`a.row[aria-label^="${lastPfx}"]`) ||
                el.querySelector('.vlist-item'),
            ) || lists.at(-1)
          );
        }
        function getScroller() {
          let el = getFolderList();
          while (el && el !== document.body && el !== document.documentElement) {
            const style = window.getComputedStyle(el);
            if (
              (style.overflowY === 'auto' || style.overflowY === 'scroll') &&
              el.scrollHeight > el.clientHeight + 2
            ) {
              return el;
            }
            el = el.parentElement;
          }
          return document.scrollingElement || document.documentElement;
        }

        const deadline = performance.now() + 90_000;
        let steps = 0;
        let idleAtBottom = 0;

        while (performance.now() < deadline) {
          const scroller = getScroller();
          const maxTop = Math.max(0, scroller.scrollHeight - scroller.clientHeight);
          const curTop = scroller.scrollTop;
          const pager = document.querySelector('.folder-pager');

          if (curTop < maxTop - 4) {
            // Jump ~2.5 viewports per animation frame, simulating fast continuous scrolling.
            const step = Math.max(scroller.clientHeight * 2.5, 1600);
            scroller.scrollTop = Math.min(maxTop, curTop + step);
            steps += 1;
            idleAtBottom = 0;
          } else if (pager) {
            // Ensure the pager is intersecting and wait a frame for the next page/batch.
            pager.scrollIntoView({ block: 'end' });
            const btn = pager.querySelector('button:not([disabled])');
            // Wait a couple of frames for IntersectionObserver first; if still idle, click Show more.
            idleAtBottom += 1;
            if (idleAtBottom > 4 && btn) {
              btn.click();
              idleAtBottom = 0;
            }
          } else {
            idleAtBottom += 1;
            if (idleAtBottom >= 3) {
              break;
            }
          }
          await new Promise((r) => requestAnimationFrame(() => r()));
        }

        const st = window.__benchPerf;
        st.running = false;
        if (st.po) st.po.disconnect();

        const frames = st.frames;
        let dropped33 = 0;
        let janky50 = 0;
        let maxFrame = 0;
        for (const dt of frames) {
          if (dt > 33.33) dropped33 += 1;
          if (dt > 50) janky50 += 1;
          if (dt > maxFrame) maxFrame = dt;
        }

        const longTasks = st.longTasks;
        const longTaskTotalMs = longTasks.reduce((a, b) => a + b, 0);
        const maxLongTaskMs = longTasks.length ? Math.max(...longTasks) : 0;
        const folderList = getFolderList();
        const rows = folderList ? [...folderList.querySelectorAll('a.row')] : [];
        const rowsAtEnd = rows.length;
        const lastRowLabel = rows.at(-1)?.getAttribute('aria-label') || rows.at(-1)?.textContent?.trim() || '';
        const domNodes = document.getElementsByTagName('*').length;
        const pagerStillPresent = Boolean(document.querySelector('.folder-pager'));
        const sawLastFile =
          lastRowLabel.startsWith(lastPfx) ||
          Boolean(folderList?.querySelector(`a.row[aria-label^="${lastPfx}"]`));

        return {
          steps,
          framesCount: frames.length,
          dropped33,
          janky50,
          maxFrame,
          longTasksCount: longTasks.length,
          longTaskTotalMs,
          maxLongTaskMs,
          rowsAtEnd,
          lastRowText: lastRowLabel,
          domNodes,
          pagerStillPresent,
          sawLastFile,
        };
      },
      { firstPfx: firstPrefix, lastPfx: lastPrefix },
    );

    const scrollTotalMs = Number(process.hrtime.bigint() - scrollT0) / 1e6;
    const m1 = readProcMetrics(env.server.pid);

    if (consoleErrors.length > 0 || pageErrors.length > 0) {
      throw new Error(
        `Browser errors during UI benchmark: console=${JSON.stringify(consoleErrors)} page=${JSON.stringify(pageErrors)}`,
      );
    }
    if (scrollResult.pagerStillPresent || !scrollResult.sawLastFile) {
      throw new Error(
        `Did not finish scrolling all 5,000 entries (rows=${scrollResult.rowsAtEnd}, last=${JSON.stringify(scrollResult.lastRowText)}, pager=${scrollResult.pagerStillPresent}, sawLast=${scrollResult.sawLastFile})`,
      );
    }

    return {
      first_paint_ms: firstPaintMs,
      initial_rows: initialRows,
      scroll_total_ms: scrollTotalMs,
      dropped_frames_33ms: scrollResult.dropped33,
      janky_frames_50ms: scrollResult.janky50,
      max_frame_ms: scrollResult.maxFrame,
      long_tasks_count: scrollResult.longTasksCount,
      long_tasks_total_ms: scrollResult.longTaskTotalMs,
      max_long_task_ms: scrollResult.maxLongTaskMs,
      final_dom_nodes: scrollResult.domNodes,
      rows_at_end: scrollResult.rowsAtEnd,
      fs_list_calls: fsListCalls,
      server_cpu_s: m1.cpuSecs != null && m0.cpuSecs != null ? Math.max(0, m1.cpuSecs - m0.cpuSecs) : null,
      server_rss_mib: m1.vmHwmMiB,
    };
  } finally {
    await context.close();
  }
}

export async function runUiBrowserSuite({ repoRoot, scratchRoot, fixtures, variants, runsOverride }) {
  const { chromium } = await loadPlaywright(repoRoot);
  const runs = runsOverride ?? 3;

  const envByVariant = new Map();
  for (const v of variants) {
    envByVariant.set(v.id, await setupUiVariant(scratchRoot, fixtures, v));
  }

  const browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  try {
    console.error(`[bench:ui-browser] desktop 1100x720 5,000-entry folder (${runs} runs x ${variants.length} variant(s))...`);
    const deskByVariant = new Map(variants.map((v) => [v.id, []]));
    for (let iter = 0; iter < runs; iter += 1) {
      const order = iter % 2 === 0 ? variants : [...variants].reverse();
      for (const v of order) {
        const sample = await measureFolderOpenAndScroll(browser, envByVariant.get(v.id), {
          viewport: { width: 1100, height: 720 },
          folderPath: fixtures.ui5k,
          firstPrefix: 'file_00000',
          lastPrefix: 'file_04999',
        });
        deskByVariant.get(v.id).push(sample);
      }
    }

    console.error(`[bench:ui-browser] phone 390x844 5,000 two-line wrapped names (${runs} runs x ${variants.length} variant(s))...`);
    const phoneByVariant = new Map(variants.map((v) => [v.id, []]));
    for (let iter = 0; iter < runs; iter += 1) {
      const order = iter % 2 === 0 ? variants : [...variants].reverse();
      for (const v of order) {
        const sample = await measureFolderOpenAndScroll(browser, envByVariant.get(v.id), {
          viewport: { width: 390, height: 844 },
          folderPath: fixtures.ui5kWrapped,
          firstPrefix: 'Quarterly_Production_Cluster_Rollout_Archive_Snapshot_Node_00000',
          lastPrefix: 'Quarterly_Production_Cluster_Rollout_Archive_Snapshot_Node_04999',
          expectWrappedTwoLines: true,
        });
        phoneByVariant.get(v.id).push(sample);
      }
    }

    const openRow = {
      id: 'ui-5k-first-paint',
      name: 'Desktop 1100×720 — 5,000-entry folder, first paint',
      highlight: true,
      note: 'In-app navigation to a 5,000-entry folder on real `legion serve` until first rows paint',
    };
    const scrollRow = {
      id: 'ui-5k-scroll-end',
      name: 'Desktop 1100×720 — 5,000-entry folder, scroll to end',
      highlight: true,
      note: 'Continuous scroll to bottom; tracks dropped frames (>33.3 ms), long tasks (>50 ms), and DOM nodes',
    };
    const phoneOpenRow = {
      id: 'ui-phone-5k-wrapped-first-paint',
      name: 'Phone 390×844 — 5,000 wrapped 2-line rows, first paint',
      highlight: true,
      note: 'Every name wraps to 2 lines on phone viewport via FileNameList',
    };
    const phoneScrollRow = {
      id: 'ui-phone-5k-wrapped-scroll-end',
      name: 'Phone 390×844 — 5,000 wrapped 2-line rows, scroll to end',
      highlight: true,
      note: 'Continuous scroll to bottom across 5,000 two-line wrapped rows on phone viewport',
    };

    for (const v of variants) {
      const dSum = summarizeSamples(deskByVariant.get(v.id));
      openRow[v.slot] = { primary_ms: dSum.first_paint_ms };
      scrollRow[v.slot] = {
        primary_ms: dSum.scroll_total_ms,
        dropped_frames_33ms: dSum.dropped_frames_33ms,
        long_tasks_total_ms: dSum.long_tasks_total_ms,
        max_frame_ms: dSum.max_frame_ms,
        final_dom_nodes: dSum.final_dom_nodes,
        fs_list_calls: dSum.fs_list_calls,
      };

      const pSum = summarizeSamples(phoneByVariant.get(v.id));
      phoneOpenRow[v.slot] = { primary_ms: pSum.first_paint_ms };
      phoneScrollRow[v.slot] = {
        primary_ms: pSum.scroll_total_ms,
        dropped_frames_33ms: pSum.dropped_frames_33ms,
        long_tasks_total_ms: pSum.long_tasks_total_ms,
        max_frame_ms: pSum.max_frame_ms,
        final_dom_nodes: pSum.final_dom_nodes,
        fs_list_calls: pSum.fs_list_calls,
      };
    }

    return {
      id: 'ui-browser',
      title: 'Web UI (`legion serve` + Headless Chromium, 5,000-entry folders)',
      description:
        'Interleaved desktop (1100×720, 5,000 rows) and phone (390×844, 5,000 two-line wrapped rows) open + continuous scroll to end on real `legion serve`.',
      columns: [
        { key: 'primary_ms', label: 'Time', unit: 'ms', decimals: 1, primary: true, higherIsBetter: false },
        { key: 'dropped_frames_33ms', label: 'Frames >33ms', unit: '', decimals: 0 },
        { key: 'long_tasks_total_ms', label: 'Long Tasks', unit: 'ms', decimals: 1 },
        { key: 'max_frame_ms', label: 'Max Frame', unit: 'ms', decimals: 1 },
        { key: 'final_dom_nodes', label: 'Final DOM Nodes', unit: '', decimals: 0 },
        { key: 'fs_list_calls', label: 'fs.list Calls', unit: '', decimals: 0 },
      ],
      rows: [openRow, scrollRow, phoneOpenRow, phoneScrollRow],
      notes: [
        'Full breakdown including `janky_frames_50ms`, `long_tasks_count`, `max_long_task_ms`, and `rows_at_end` is saved in results.json.',
      ],
    };
  } finally {
    await browser.close();
    for (const env of envByVariant.values()) {
      await env.server.stop();
    }
  }
}
