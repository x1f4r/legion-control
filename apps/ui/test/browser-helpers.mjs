// Shared browser test helpers: bounded test timeouts, bounded waits with step/URL/console
// diagnostics, deterministic media & layout waits, and leak-free server lifecycle helpers.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import readline from 'node:readline';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

export const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
export const UI_ROOT = path.resolve(TEST_DIR, '..');
export const REPO_ROOT = path.resolve(TEST_DIR, '..', '..', '..');
export const LEGION_BIN =
  process.env.LEGION_BIN ||
  path.join(REPO_ROOT, 'target', 'release', process.platform === 'win32' ? 'legion.exe' : 'legion');

export const DEFAULT_WAIT_MS = 15_000;
export const DEFAULT_NAV_MS = 20_000;
export const DEFAULT_TEST_TIMEOUT_MS = 90_000;

// Same strict CSP as the HTTP door plus `media-src 'self' blob: data:` for mock sample video/audio.
export const MOCK_TEST_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self' blob: data:; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

export const MIME_BY_EXT = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json; charset=utf-8',
  '.webmanifest': 'application/manifest+json; charset=utf-8',
};

export function findBrowser() {
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

export async function launchBrowser({ autoplay = false, extraArgs = [] } = {}) {
  const args = ['--no-sandbox', '--disable-gpu'];
  if (autoplay) args.push('--autoplay-policy=no-user-gesture-required');
  args.push(...extraArgs);
  return chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args,
  });
}

function safePageUrl(page) {
  if (!page) return '(no page)';
  try {
    return page.isClosed() ? '(page closed)' : page.url() || '(empty url)';
  } catch {
    return '(unavailable)';
  }
}

export function createTestTracker({ getServerStderr = null } = {}) {
  let currentStep = 'setup';
  let activePage = null;
  let lastKnownUrl = '(no page)';
  let serverStderrGetter = getServerStderr;
  const consoleErrors = [];
  const pageErrors = [];

  function rememberUrl(page) {
    const p = page || activePage;
    if (!p) return;
    try {
      if (!p.isClosed()) {
        const u = p.url();
        if (u) lastKnownUrl = u;
      }
    } catch {
      // Keep lastKnownUrl.
    }
  }

  function setStep(label) {
    if (label) currentStep = label;
    rememberUrl();
  }

  function setServerStderr(getter) {
    serverStderrGetter = getter;
  }

  function attach(page, { label = '', collectConsole = null, collectPage = null } = {}) {
    activePage = page;
    rememberUrl(page);
    page.setDefaultTimeout(DEFAULT_WAIT_MS);
    page.setDefaultNavigationTimeout(DEFAULT_NAV_MS);
    page.on('framenavigated', (frame) => {
      if (frame === page.mainFrame()) {
        const u = frame.url();
        if (u) lastKnownUrl = u;
      }
    });
    page.on('console', (msg) => {
      if (msg.type() === 'error') {
        const text = label ? `[${label}] ${msg.text()}` : msg.text();
        consoleErrors.push(text);
        if (Array.isArray(collectConsole)) collectConsole.push(text);
      }
    });
    page.on('pageerror', (err) => {
      const text = label ? `[${label}] ${err}` : String(err);
      pageErrors.push(text);
      if (Array.isArray(collectPage)) collectPage.push(text);
    });
    return page;
  }

  function formatDiagnostics({ step = null, page = null, extra = null } = {}) {
    const p = page || activePage;
    const liveUrl = safePageUrl(p);
    const displayUrl =
      liveUrl && !liveUrl.startsWith('(') ? liveUrl : lastKnownUrl !== '(no page)' ? `${lastKnownUrl} (closed)` : liveUrl;
    const recentConsole = consoleErrors.slice(-5);
    const recentPage = pageErrors.slice(-5);
    const lines = [
      `  step: ${step || currentStep}`,
      `  url: ${displayUrl}`,
      `  last console errors: ${recentConsole.length ? recentConsole.join(' | ') : '(none)'}`,
    ];
    if (recentPage.length) {
      lines.push(`  last page errors: ${recentPage.join(' | ')}`);
    }
    if (extra) {
      lines.push(`  detail: ${typeof extra === 'string' ? extra : JSON.stringify(extra)}`);
    }
    if (typeof serverStderrGetter === 'function') {
      try {
        const raw = String(serverStderrGetter() || '').trim();
        if (raw) {
          const tail = raw.split('\n').slice(-8).join('\n    ');
          lines.push(`  server stderr (tail):\n    ${tail}`);
        }
      } catch {
        // Ignore stderr read failures during error formatting.
      }
    }
    return lines.join('\n');
  }

  function annotateError(err, { step = null, page = null, extra = null } = {}) {
    if (err && typeof err === 'object' && err.__legionAnnotated) return err;
    const baseMsg = err instanceof Error ? err.message : String(err);
    const wrapped = new Error(`${baseMsg}\n${formatDiagnostics({ step, page, extra })}`, {
      cause: err instanceof Error ? err : undefined,
    });
    wrapped.name = err instanceof Error && err.name ? err.name : 'Error';
    wrapped.__legionAnnotated = true;
    return wrapped;
  }

  async function waitForLocator(locator, step, opts = {}) {
    setStep(step);
    const page = opts.page || locator.page?.() || activePage;
    const { page: _ignored, ...waitOpts } = opts;
    try {
      await locator.waitFor({ timeout: DEFAULT_WAIT_MS, ...waitOpts });
    } catch (err) {
      throw annotateError(err, { step: `waiting for ${step}`, page });
    }
  }

  async function waitForFn(page, fn, arg, step, opts = {}) {
    setStep(step);
    activePage = page;
    try {
      return await page.waitForFunction(fn, arg, { timeout: DEFAULT_WAIT_MS, ...opts });
    } catch (err) {
      throw annotateError(err, { step: `waiting for ${step}`, page });
    }
  }

  async function waitForResponse(page, predicate, step, opts = {}) {
    setStep(step);
    activePage = page;
    try {
      return await page.waitForResponse(predicate, { timeout: DEFAULT_WAIT_MS, ...opts });
    } catch (err) {
      throw annotateError(err, { step: `waiting for response (${step})`, page });
    }
  }

  async function waitForCondition(step, predicate, { timeoutMs = DEFAULT_WAIT_MS, intervalMs = 100, page = null } = {}) {
    setStep(step);
    const deadline = Date.now() + timeoutMs;
    let lastValue = null;
    while (true) {
      lastValue = await predicate();
      if (lastValue) return lastValue;
      if (Date.now() >= deadline) {
        throw annotateError(
          new Error(`Timed out after ${timeoutMs}ms waiting for ${step}`),
          { step, page, extra: lastValue !== false && lastValue != null ? { lastValue } : null },
        );
      }
      await new Promise((resolve) => setTimeout(resolve, Math.min(intervalMs, Math.max(10, deadline - Date.now()))));
    }
  }

  function makeBoundedGate(name, { timeoutMs = DEFAULT_WAIT_MS } = {}) {
    let releaseFn;
    let arrivedResolve;
    let isArrived = false;
    let held = true;
    const releasePromise = new Promise((r) => {
      releaseFn = r;
    });
    const arrivedPromise = new Promise((r) => {
      arrivedResolve = r;
    });
    return {
      isHeld: () => held,
      markArrived() {
        isArrived = true;
        arrivedResolve();
      },
      async waitArrived(step = `${name} request to arrive`) {
        setStep(step);
        if (isArrived) return;
        let timer = null;
        try {
          await Promise.race([
            arrivedPromise,
            new Promise((_, reject) => {
              timer = setTimeout(() => {
                reject(
                  annotateError(
                    new Error(`Timed out after ${timeoutMs}ms waiting for ${step}`),
                    { step },
                  ),
                );
              }, timeoutMs);
            }),
          ]);
        } finally {
          if (timer !== null) clearTimeout(timer);
        }
      },
      waitRelease: () => releasePromise,
      release() {
        held = false;
        releaseFn();
      },
    };
  }

  return {
    consoleErrors,
    pageErrors,
    setStep,
    setServerStderr,
    attach,
    formatDiagnostics,
    annotateError,
    waitForLocator,
    waitForFn,
    waitForResponse,
    waitForCondition,
    makeBoundedGate,
  };
}

export function browserTest(name, optionsOrFn, maybeFn) {
  const opts = typeof optionsOrFn === 'function' ? {} : optionsOrFn || {};
  const fn = typeof optionsOrFn === 'function' ? optionsOrFn : maybeFn;
  const timeout = opts.timeout ?? DEFAULT_TEST_TIMEOUT_MS;
  return test(name, { ...opts, timeout }, async (t) => {
    const tracker = createTestTracker();
    try {
      await fn(t, tracker);
    } catch (err) {
      throw tracker.annotateError(err);
    }
  });
}

export async function waitForLayoutSettled(page, frames = 2, timeoutMs = 2_000) {
  await page.evaluate(
    ({ count, timeout }) =>
      new Promise((resolve) => {
        let done = false;
        const finish = () => {
          if (done) return;
          done = true;
          clearTimeout(timer);
          resolve();
        };
        const timer = setTimeout(finish, timeout);
        let remaining = Math.max(1, count);
        const tick = () => {
          remaining -= 1;
          if (remaining <= 0) finish();
          else requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      }),
    { count: frames, timeout: timeoutMs },
  );
}

export async function setTheme(page, theme, tracker = null) {
  if (tracker) tracker.setStep(`set theme to ${theme}`);
  await page.evaluate((t) => {
    const raw = localStorage.getItem('legion.settings.v1');
    const obj = raw ? JSON.parse(raw) : {};
    obj.theme = t;
    localStorage.setItem('legion.settings.v1', JSON.stringify(obj));
    document.documentElement.dataset.theme = t;
  }, theme);
  await waitForLayoutSettled(page, 2);
}

export async function waitForImageLoaded(locator, step, { tracker = null, timeoutMs = DEFAULT_WAIT_MS } = {}) {
  const page = locator.page?.();
  if (tracker) tracker.setStep(step);
  try {
    await locator.waitFor({ timeout: timeoutMs });
    return await locator.evaluate(
      (el, { timeout, label }) =>
        new Promise((resolve, reject) => {
          const snapshot = () => ({
            src: el.getAttribute('src') || '',
            complete: el.complete,
            width: el.naturalWidth,
            height: el.naturalHeight,
            connected: el.isConnected,
          });
          if (el.complete && el.naturalWidth > 0 && el.naturalHeight > 0) {
            resolve(snapshot());
            return;
          }
          let done = false;
          const finish = (ok, err) => {
            if (done) return;
            done = true;
            clearTimeout(timer);
            el.removeEventListener('load', onLoad);
            el.removeEventListener('error', onError);
            if (ok) resolve(snapshot());
            else reject(err);
          };
          const onLoad = () => {
            if (el.naturalWidth > 0 && el.naturalHeight > 0) finish(true);
            else finish(false, new Error(`Image loaded with zero size (${label}): ${JSON.stringify(snapshot())}`));
          };
          const onError = () => {
            finish(false, new Error(`Image failed to load (${label}): ${JSON.stringify(snapshot())}`));
          };
          const timer = setTimeout(() => {
            finish(false, new Error(`Timed out after ${timeout}ms waiting for ${label}: ${JSON.stringify(snapshot())}`));
          }, timeout);
          el.addEventListener('load', onLoad, { once: true });
          el.addEventListener('error', onError, { once: true });
        }),
      { timeout: timeoutMs, label: step },
    );
  } catch (err) {
    throw tracker ? tracker.annotateError(err, { step, page }) : err;
  }
}

export async function waitForMediaReady(
  locator,
  step,
  {
    tracker = null,
    minReadyState = 2,
    forceAutoPreload = true,
    timeoutMs = DEFAULT_WAIT_MS,
  } = {},
) {
  const page = locator.page?.();
  if (tracker) tracker.setStep(step);
  try {
    await locator.waitFor({ state: 'attached', timeout: timeoutMs });
    return await locator.evaluate(
      (el, { minState, forceAuto, timeout, label }) =>
        new Promise((resolve, reject) => {
          const snapshot = () => ({
            src: el.getAttribute('src') || '',
            readyState: el.readyState,
            networkState: el.networkState,
            currentTime: el.currentTime,
            duration: el.duration,
            paused: el.paused,
            ended: el.ended,
            errorCode: el.error?.code ?? null,
            errorMessage: el.error?.message ?? null,
            connected: el.isConnected,
          });
          if (el.error) {
            reject(new Error(`Media error before ready (${label}): ${JSON.stringify(snapshot())}`));
            return;
          }
          if (el.readyState >= minState) {
            resolve(snapshot());
            return;
          }
          if (forceAuto && el.preload !== 'auto') {
            el.preload = 'auto';
          }
          let done = false;
          const events = ['loadedmetadata', 'loadeddata', 'canplay', 'canplaythrough'];
          const cleanup = () => {
            clearTimeout(timer);
            clearInterval(watch);
            for (const ev of events) el.removeEventListener(ev, onReady);
            el.removeEventListener('error', onError);
          };
          const finish = (ok, err) => {
            if (done) return;
            done = true;
            cleanup();
            if (ok) resolve(snapshot());
            else reject(err);
          };
          const onReady = () => {
            if (el.error) {
              finish(false, new Error(`Media error while loading (${label}): ${JSON.stringify(snapshot())}`));
            } else if (el.readyState >= minState) {
              finish(true);
            }
          };
          const onError = () => {
            finish(false, new Error(`Media error (${label}): ${JSON.stringify(snapshot())}`));
          };
          const watch = setInterval(() => {
            if (!el.isConnected) {
              finish(false, new Error(`Media element detached while waiting for ${label}: ${JSON.stringify(snapshot())}`));
            } else if (el.error) {
              onError();
            } else if (el.readyState >= minState) {
              finish(true);
            }
          }, 100);
          const timer = setTimeout(() => {
            finish(false, new Error(`Timed out after ${timeout}ms waiting for ${label}: ${JSON.stringify(snapshot())}`));
          }, timeout);
          for (const ev of events) el.addEventListener(ev, onReady);
          el.addEventListener('error', onError, { once: true });
          // Nudge loading only if src is already present and preload=metadata paused at network idle.
          if (forceAuto && el.getAttribute('src') && el.readyState < minState && el.networkState === 1) {
            try {
              el.load();
            } catch {
              // Ignore reload errors.
            }
          }
        }),
      { minState: minReadyState, forceAuto: forceAutoPreload, timeout: timeoutMs, label: step },
    );
  } catch (err) {
    throw tracker ? tracker.annotateError(err, { step, page }) : err;
  }
}

export async function playAndWaitForTime(
  locator,
  targetTime,
  step,
  { tracker = null, timeoutMs = DEFAULT_WAIT_MS, pauseAfter = true, recordRecoveryAudit = false } = {},
) {
  const page = locator.page?.();
  if (tracker) tracker.setStep(step);
  try {
    await locator.waitFor({ state: 'attached', timeout: timeoutMs });
    return await locator.evaluate(
      async (el, { target, timeout, pauseAfter: shouldPause, audit, label }) => {
        const snapshot = () => ({
          src: el.getAttribute('src') || '',
          readyState: el.readyState,
          networkState: el.networkState,
          currentTime: el.currentTime,
          duration: el.duration,
          paused: el.paused,
          ended: el.ended,
          bufferedEnd: el.buffered.length > 0 ? el.buffered.end(0) : 0,
          errorCode: el.error?.code ?? null,
          errorMessage: el.error?.message ?? null,
          connected: el.isConnected,
        });
        el.muted = true;
        if (el.readyState < 2 && el.preload !== 'auto') {
          el.preload = 'auto';
        }
        await new Promise((resolve, reject) => {
          if (el.error) {
            reject(new Error(`Initial media error (${label}): ${JSON.stringify(snapshot())}`));
            return;
          }
          if (el.readyState >= 2) {
            resolve();
            return;
          }
          let done = false;
          const finish = (ok, err) => {
            if (done) return;
            done = true;
            clearTimeout(t);
            clearInterval(watch);
            el.removeEventListener('loadeddata', onData);
            el.removeEventListener('canplay', onData);
            el.removeEventListener('error', onErr);
            if (ok) resolve();
            else reject(err);
          };
          const onData = () => {
            if (el.readyState >= 2) finish(true);
          };
          const onErr = () => finish(false, new Error(`Initial media error (${label}): ${JSON.stringify(snapshot())}`));
          const watch = setInterval(() => {
            if (!el.isConnected) finish(false, new Error(`Media detached before loadeddata (${label}): ${JSON.stringify(snapshot())}`));
            else if (el.error) onErr();
            else if (el.readyState >= 2) finish(true);
          }, 100);
          const t = setTimeout(
            () => finish(false, new Error(`Timed out after ${timeout}ms waiting for loadeddata (${label}): ${JSON.stringify(snapshot())}`)),
            timeout,
          );
          el.addEventListener('loadeddata', onData);
          el.addEventListener('canplay', onData);
          el.addEventListener('error', onErr, { once: true });
        });

        await el.play();

        await new Promise((resolve, reject) => {
          if (el.currentTime >= target) {
            resolve();
            return;
          }
          let done = false;
          const finish = (ok, err) => {
            if (done) return;
            done = true;
            clearTimeout(t);
            clearInterval(watch);
            el.removeEventListener('timeupdate', check);
            el.removeEventListener('error', onErr);
            if (ok) resolve();
            else reject(err);
          };
          const check = () => {
            if (el.currentTime >= target) finish(true);
          };
          const onErr = () => finish(false, new Error(`Media error during playback (${label}): ${JSON.stringify(snapshot())}`));
          const watch = setInterval(() => {
            if (!el.isConnected) {
              finish(false, new Error(`Media detached during playback (${label}): ${JSON.stringify(snapshot())}`));
            } else if (el.error) {
              onErr();
            } else if (el.currentTime >= target) {
              finish(true);
            }
          }, 100);
          const t = setTimeout(
            () =>
              finish(
                false,
                new Error(`Timed out after ${timeout}ms waiting for currentTime >= ${target}s (${label}): ${JSON.stringify(snapshot())}`),
              ),
            timeout,
          );
          el.addEventListener('timeupdate', check);
          el.addEventListener('error', onErr, { once: true });
          check();
        });

        if (shouldPause) el.pause();
        if (audit) {
          window.__recoveryAudit = { timeAtError: null, timeAtSwap: null };
          el.addEventListener(
            'error',
            () => {
              window.__recoveryAudit.timeAtError = el.currentTime;
            },
            { once: true },
          );
        }
        return {
          src: el.getAttribute('src') || '',
          pausedAt: el.currentTime,
          bufferedEnd: el.buffered.length > 0 ? el.buffered.end(0) : 0,
        };
      },
      { target: targetTime, timeout: timeoutMs, pauseAfter, audit: recordRecoveryAudit, label: step },
    );
  } catch (err) {
    throw tracker ? tracker.annotateError(err, { step, page }) : err;
  }
}

export async function seekAndWait(locator, targetTime, step, { tracker = null, timeoutMs = DEFAULT_WAIT_MS } = {}) {
  const page = locator.page?.();
  if (tracker) tracker.setStep(step);
  try {
    return await locator.evaluate(
      async (el, { target, timeout, label }) => {
        const snapshot = () => ({
          src: el.getAttribute('src') || '',
          readyState: el.readyState,
          networkState: el.networkState,
          currentTime: el.currentTime,
          duration: el.duration,
          seeking: el.seeking,
          paused: el.paused,
          width: el.videoWidth ?? 0,
          height: el.videoHeight ?? 0,
          errorCode: el.error?.code ?? null,
          errorMessage: el.error?.message ?? null,
          connected: el.isConnected,
        });
        const seeked = new Promise((resolve, reject) => {
          let done = false;
          const finish = (ok, err) => {
            if (done) return;
            done = true;
            clearTimeout(t);
            el.removeEventListener('seeked', onSeeked);
            el.removeEventListener('error', onErr);
            if (ok) resolve();
            else reject(err);
          };
          const onSeeked = () => finish(true);
          const onErr = () => finish(false, new Error(`Media seek error (${label}): ${JSON.stringify(snapshot())}`));
          const t = setTimeout(
            () => finish(false, new Error(`Timed out after ${timeout}ms waiting for seeked (${label}): ${JSON.stringify(snapshot())}`)),
            timeout,
          );
          el.addEventListener('seeked', onSeeked, { once: true });
          el.addEventListener('error', onErr, { once: true });
        });
        el.currentTime = target;
        await seeked;
        return {
          src: el.getAttribute('src') || '',
          readyState: el.readyState,
          duration: el.duration,
          width: el.videoWidth ?? 0,
          height: el.videoHeight ?? 0,
          currentTime: el.currentTime,
          hasError: el.error !== null,
          err: el.error,
        };
      },
      { target: targetTime, timeout: timeoutMs, label: step },
    );
  } catch (err) {
    throw tracker ? tracker.annotateError(err, { step, page }) : err;
  }
}

export async function closeHttpServer(server, timeoutMs = 5_000) {
  if (!server) return;
  try {
    server.closeIdleConnections?.();
    server.closeAllConnections?.();
  } catch {
    // Ignore closeAllConnections errors on already-closed servers.
  }
  await new Promise((resolve) => {
    let settled = false;
    const done = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(done, timeoutMs);
    try {
      server.close(done);
    } catch {
      done();
    }
  });
}

export async function startStaticUiServer({ uiRoot = UI_ROOT, extraHandler = null, cacheControl = 'no-store' } = {}) {
  const server = http.createServer((req, res) => {
    if (extraHandler && extraHandler(req, res) === true) return;
    const parsed = new URL(req.url || '/', 'http://127.0.0.1');
    let rel;
    try {
      rel = decodeURIComponent(parsed.pathname);
    } catch {
      res.writeHead(400);
      res.end('Bad request');
      return;
    }
    if (rel === '/') rel = '/index.html';
    const full = path.resolve(uiRoot, `.${rel}`);
    if (!full.startsWith(uiRoot) || !fs.existsSync(full) || fs.statSync(full).isDirectory()) {
      res.writeHead(404);
      res.end('Not found');
      return;
    }
    const ext = path.extname(full).toLowerCase();
    const headers = {
      'Content-Type': MIME_BY_EXT[ext] || 'application/octet-stream',
      'X-Content-Type-Options': 'nosniff',
    };
    if (cacheControl) headers['Cache-Control'] = cacheControl;
    if (ext === '.html') headers['Content-Security-Policy'] = MOCK_TEST_CSP;
    res.writeHead(200, headers);
    fs.createReadStream(full).pipe(res);
  });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Static UI server did not start listening within 10s')), 10_000);
    server.once('error', (err) => {
      clearTimeout(timer);
      reject(err);
    });
    server.listen(0, '127.0.0.1', () => {
      clearTimeout(timer);
      resolve();
    });
  });
  const addr = server.address();
  return {
    url: `http://127.0.0.1:${addr.port}`,
    server,
    stop: () => closeHttpServer(server),
  };
}

export async function startLegionServer(
  legionHome,
  userHome,
  { listen = '127.0.0.1:0', envExtra = {}, startupTimeoutMs = 20_000, legionBin = LEGION_BIN } = {},
) {
  const env = {
    ...process.env,
    ...envExtra,
    LEGION_HOME: legionHome,
    HOME: userHome,
    USERPROFILE: userHome,
  };
  delete env.XDG_CONFIG_HOME;
  delete env.XDG_STATE_HOME;

  const child = spawn(legionBin, ['serve', '--listen', listen], {
    env,
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  const rl = readline.createInterface({ input: child.stderr });

  async function killChild() {
    try {
      rl.close();
    } catch {
      // Ignore readline close failures.
    }
    if (child.exitCode !== null || child.signalCode !== null) return;
    try {
      child.kill('SIGTERM');
    } catch {
      return;
    }
    await new Promise((resolve) => {
      let settled = false;
      const done = () => {
        if (settled) return;
        settled = true;
        clearTimeout(t);
        resolve();
      };
      const t = setTimeout(() => {
        try {
          child.kill('SIGKILL');
        } catch {
          // Process already gone.
        }
        done();
      }, 8_000);
      child.once('exit', done);
    });
  }

  let url;
  try {
    url = await new Promise((resolve, reject) => {
      let settled = false;
      const settle = (ok, value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (ok) resolve(value);
        else reject(value);
      };
      const timer = setTimeout(() => {
        settle(false, new Error(`legion serve did not announce its port within ${startupTimeoutMs}ms. stderr:\n${stderr}`));
      }, startupTimeoutMs);
      rl.on('line', (line) => {
        stderr += `${line}\n`;
        const prefix = 'legion serve: listening on ';
        if (line.startsWith(prefix)) {
          settle(true, line.slice(prefix.length).trim());
        }
      });
      child.once('error', (err) => {
        settle(false, new Error(`legion serve failed to spawn: ${err.message}\nstderr:\n${stderr}`));
      });
      child.once('exit', (code, signal) => {
        settle(false, new Error(`legion serve exited early (code=${code}, signal=${signal}). stderr:\n${stderr}`));
      });
    });
  } catch (err) {
    await killChild();
    throw err;
  }

  return {
    url,
    child,
    stop: killChild,
    getStderr: () => stderr,
  };
}
