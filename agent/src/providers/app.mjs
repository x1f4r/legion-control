// Desktop apps that update themselves.
//
// This kind is genuinely a different animal from a package we install, and the
// difference is worth stating up front, because almost every mistake in here
// would come from assuming otherwise:
//
//   An npm service is ours to update. npm install rewrites node_modules and we
//   restart a process around it.
//
//   An app of this kind updates ITSELF (electron-updater and the like): it polls
//   its own release feed, downloads the new build on its own, and stages it in
//   the updater cache. It then applies the staged build when the app quits
//   (autoInstallOnAppQuit, on by default).
//
// So the only step that is ever missing is the quit. We must never download
// anything, never unpack a zip, never touch the app bundle. The whole job is to
// notice that something is staged and, at a safe moment, ask the app to quit and
// start it again.
//
// The quit is asked for with AppleScript and NOTHING ELSE. No kill, no pkill, no
// SIGTERM. A forced kill throws away whatever the user had open, and it can also
// leave the updater half way through swapping the bundle. If the app will not go
// away politely, the correct answer is to give up and say so.

import fs from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { compilePattern, describeFailure, runCommand, sleep, LEGACY_DEFAULTS } from '../config.mjs';
import { cachedLookup, channelCacheKey } from './npm.mjs';

// How long we are willing to wait for a polite quit before giving up. An app
// normally goes in a couple of seconds; the long tail is a confirmation dialog
// or a shutdown hook, and neither is something we may override.
export const QUIT_WAIT_MS = 60000;
const QUIT_POLL_MS = 1000;
const LAUNCH_GRACE_MS = 2000;

export function appPath(service) {
  return service.path;
}

export function pendingDir(service) {
  return path.join(service.updaterCacheDir, 'pending');
}

/**
 * The version of the bundle on disk, from its Info.plist. PlistBuddy is part of
 * the base system, so there is nothing to install and nothing to parse by hand.
 *
 * We only ever read the bundle the config names, and a missing or unreadable
 * plist returns null rather than throwing, so a stray bundle sitting next to the
 * real one can neither be picked by accident nor crash us.
 */
export function installedVersion(service) {
  if (!service.path) return null;
  const plist = path.join(service.path, 'Contents', 'Info.plist');
  try {
    if (!fs.existsSync(plist)) return null;
  } catch {
    return null;
  }
  const result = runCommand('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleShortVersionString', plist], {
    timeoutMs: 10000,
  });
  if (!result.ok) return null;
  const version = result.stdout.trim();
  return version.length > 0 ? version : null;
}

/**
 * The version the updater has already downloaded and is holding for the next
 * quit, or null when there is nothing waiting.
 *
 * Worth knowing: pending/ is NOT cleared once a staged build has been applied.
 * The archive for the version that is now running just stays there. So the file
 * existing proves nothing on its own, and the only honest test for "an update is
 * waiting" is stagedVersion() !== installedVersion(). Equal means it already
 * went in.
 */
export function stagedVersion(service) {
  if (!service.updaterCacheDir) return null;
  const infoFile = path.join(pendingDir(service), 'update-info.json');
  let fileName;
  try {
    fileName = JSON.parse(fs.readFileSync(infoFile, 'utf8'))?.fileName;
  } catch {
    // Missing directory, missing file, truncated JSON: all of them mean the same
    // thing to a caller, which is that we cannot name a staged version.
    return null;
  }
  if (typeof fileName !== 'string') return null;
  const pattern = compilePattern(service.stagedFilePattern, LEGACY_DEFAULTS.stagedFilePattern);
  if (!pattern) return null;
  const match = pattern.exec(path.basename(fileName).trim());
  return match && typeof match[1] === 'string' && match[1].length > 0 ? match[1] : null;
}

/** GET an https URL and return the body, or null. Never rejects. */
function getBody(url, timeoutMs) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };

    const request = https.get(
      url,
      {
        timeout: timeoutMs,
        headers: {
          // The GitHub API rejects requests without a User-Agent outright.
          'user-agent': 'legionctl',
          accept: 'application/vnd.github+json',
          connection: 'close',
        },
      },
      (response) => {
        const status = response.statusCode ?? 0;
        if (status !== 200) {
          response.resume();
          finish({ body: null, error: `GitHub answered HTTP ${status}` });
          return;
        }
        let body = '';
        let overflowed = false;
        response.setEncoding('utf8');
        response.on('data', (chunk) => {
          if (overflowed) return;
          body += chunk;
          // Release bodies are prose and can be long. This is only a guard
          // against an unexpectedly huge response, not a real limit.
          if (body.length > 8 * 1024 * 1024) {
            overflowed = true;
            request.destroy();
            finish({ body: null, error: 'the GitHub response was implausibly large' });
          }
        });
        response.on('end', () => finish({ body, error: null }));
        response.on('error', (err) => finish({ body: null, error: err.message }));
      },
    );
    request.on('timeout', () => {
      request.destroy();
      finish({ body: null, error: `GitHub timed out after ${timeoutMs} ms` });
    });
    request.on('error', (err) => finish({ body: null, error: err.message }));
  });
}

/** Pick the newest usable tag out of a GitHub releases listing. Pure. */
export function pickRelease(releases, { prerelease, pattern }) {
  if (!Array.isArray(releases)) return { version: null, error: 'the GitHub releases response is not a list' };
  // The API returns newest first, so the first matching release that is not a
  // draft is the one the app's own channel would pick up.
  const release = releases.find((r) => r && !r.draft && Boolean(r.prerelease) === Boolean(prerelease));
  if (!release) return { version: null, error: 'no matching release in the newest 20' };

  const tag = String(release.tag_name ?? '')
    .trim()
    .replace(/^v/, '');
  // Only something version-shaped is reported, so a change of tagging style
  // degrades to "unknown" instead of putting a nonsense string in front of the
  // user.
  if (pattern && !pattern.test(tag)) return { version: null, error: `unexpected release tag: ${JSON.stringify(tag)}` };
  return { version: tag, error: null };
}

/**
 * The newest release the app's own feed announces. REPORTING ONLY: nothing is
 * ever installed from it. It exists so `status` can say "the app is a build
 * behind and has not downloaded it yet", which is otherwise invisible.
 *
 * Any failure at all (offline, rate limited, garbage JSON) degrades to null.
 * Callers must read null as "cannot tell", never as "nothing new".
 */
export async function fetchLatestVersion(service, { timeoutMs = 8000 } = {}) {
  const latest = service.latest;
  if (!latest) return { version: null, error: 'no release feed is configured for this service' };

  const url = `https://api.github.com/repos/${latest.repo}/releases?per_page=20`;
  const { body, error } = await getBody(url, timeoutMs);
  if (body === null) return { version: null, error };

  let releases;
  try {
    releases = JSON.parse(body);
  } catch {
    return { version: null, error: 'the GitHub releases response is not JSON' };
  }
  return pickRelease(releases, {
    prerelease: latest.prerelease,
    pattern: compilePattern(service.versionPattern, null),
  });
}

/**
 * fetchLatestVersion with the same 10 minute state.json cache the npm side uses,
 * so `status` stays fast and we stay well clear of GitHub's unauthenticated rate
 * limit. The cache key is tagged with the source because a release feed is a
 * different thing from an npm dist-tag.
 */
export function cachedLatestVersion(service, options = {}) {
  return cachedLookup(channelCacheKey(service, 'github'), options, () =>
    fetchLatestVersion(service, { timeoutMs: options.timeoutMs ?? 4000 }),
  );
}

/**
 * Every live process belonging to the bundle: the main process and all the
 * helpers, which sit under Contents/Frameworks. Matching the bundle path as a
 * plain string rather than handing it to pgrep is deliberate, since an app name
 * like "Some App (Nightly).app" is full of characters pgrep would read as a
 * regular expression. It also means a similarly named bundle cannot match.
 *
 * "comm=" and a prefix test, NOT "command=" and a substring test. comm is the
 * executable's own path; command is the whole argv, and any process on the
 * machine can put the bundle path in its argv just by mentioning it. A shell
 * running `ls "<bundle>/Contents/"`, a grep over the bundle, or a codesign check
 * all matched the old test, which made an app that had already quit look like it
 * was still there: quitAndWait would then burn its full timeout and report a
 * perfectly good update as failed, leaving the app closed instead of restarting
 * it. An executable path cannot be faked that way.
 */
export function appPids(service) {
  if (!service.path) return { pids: [], error: 'no app path is configured for this service' };
  const needle = `${service.path}/Contents/`;
  const result = runCommand('/bin/ps', ['-Ao', 'pid=,comm='], { timeoutMs: 15000 });
  if (!result.ok) return { pids: [], error: describeFailure(result) };

  const pids = [];
  for (const line of result.stdout.split('\n')) {
    const match = /^\s*(\d+)\s+(.*)$/.exec(line);
    if (!match) continue;
    const pid = Number.parseInt(match[1], 10);
    if (pid === process.pid) continue;
    if (match[2].startsWith(needle)) pids.push(pid);
  }
  return { pids, error: null };
}

export function isAppRunning(service) {
  const { pids, error } = appPids(service);
  // Fail closed, the same way the process probe does elsewhere: a probe we could
  // not complete has to read as "still running", because the caller uses this to
  // decide whether it is safe to launch a second copy.
  if (error) return true;
  return pids.length > 0;
}

/**
 * Ask the app to quit, politely. This is the step that lets the updater swap in
 * a staged build, so the graceful path is not a nicety, it is the whole
 * mechanism.
 */
export function askAppToQuit(service) {
  if (!service.bundleId) return { ok: false, message: 'no bundle id is configured for this service' };
  const result = runCommand('osascript', ['-e', `quit app id "${service.bundleId}"`], { timeoutMs: 30000 });
  return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
}

/**
 * Ask, then wait for every process in the bundle to be gone. On timeout we
 * report failure and leave the app exactly as it is. We do NOT escalate.
 */
export async function quitAndWait(service, timeoutMs = QUIT_WAIT_MS) {
  if (!isAppRunning(service)) return { ok: true, notRunning: true, message: 'the app was not running' };

  const asked = askAppToQuit(service);
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!isAppRunning(service)) return { ok: true, message: null };
    // A quit request that was refused outright is not going to start working by
    // being waited on. Give it a couple of polls in case osascript reported a
    // transient error while the quit still went through, then stop: sitting here
    // for the full minute only delays the honest answer.
    if (!asked.ok && Date.now() > deadline - timeoutMs + QUIT_POLL_MS * 2) {
      return {
        ok: false,
        message: `the quit request failed (${asked.message}) and ${service.name} is still running; it was left alone rather than forced`,
      };
    }
    await sleep(QUIT_POLL_MS);
  }

  const waited = Math.round(timeoutMs / 1000);
  return {
    ok: false,
    message: asked.ok
      ? `${service.name} did not quit within ${waited} s; it was left running on purpose, since forcing it would lose open work and could leave a staged update half applied`
      : `the quit request failed (${asked.message}) and ${service.name} is still running after ${waited} s; it was left alone rather than forced`,
  };
}

export function launchApp(service) {
  if (!service.path) return { ok: false, message: 'no app path is configured for this service' };
  const result = runCommand('open', ['-a', service.path], { timeoutMs: 30000 });
  return { ok: result.ok, message: result.ok ? null : describeFailure(result) };
}

/** Launch and give the app a moment to bind its port before anyone health-checks it. */
export async function launchAndSettle(service) {
  const started = launchApp(service);
  await sleep(LAUNCH_GRACE_MS);
  return started;
}

/**
 * Quit, confirm it is really gone, start it again. Health is the caller's
 * business, because both callers want to phrase the failure differently.
 *
 * launchIfStopped defaults to false on purpose. A background job that finds the
 * app already closed must not open it: the user closed it deliberately, and
 * having a 15 minute timer reopen an app is precisely the kind of thing that
 * makes people uninstall the timer. A staged build does not need us to do this
 * anyway, since the updater applies it the next time the app itself quits.
 */
export async function restartApp(service, { timeoutMs = QUIT_WAIT_MS, launchIfStopped = false } = {}) {
  const quit = await quitAndWait(service, timeoutMs);
  if (!quit.ok) return quit;
  if (quit.notRunning && !launchIfStopped) {
    return { ok: true, notRunning: true, message: `${service.name} was not running, so it was left closed` };
  }
  return launchAndSettle(service);
}

/**
 * After a relaunch, wait for the bundle to actually report the new version.
 *
 * The updater swaps the bundle on quit, and reading Info.plist once right
 * afterwards can catch that swap mid flight and read the old version (or fail to
 * read it at all), which would be recorded as "the update did not apply" for an
 * update that applied perfectly well.
 */
export async function waitForVersion(service, expected, timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs;
  let seen = null;
  for (;;) {
    seen = installedVersion(service);
    if (seen === expected) return { ok: true, version: seen };
    if (Date.now() >= deadline) return { ok: false, version: seen };
    await sleep(2000);
  }
}
