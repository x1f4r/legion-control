// Services described entirely by commands: one argv array to read the installed
// version, one to ask what the newest version is, one to install it.
//
// All three are optional, and what is missing narrows what the apps may offer.
// Without installedVersion there is no version to show; without latestVersion
// the service reports upToDate: null and the apps say the version was not
// checked; without update the service can only be restarted.

import { compilePattern, describeFailure, runArgv } from '../config.mjs';
import { runMutationArgv } from '../process-runner.mjs';
import { cachedLookup, channelCacheKey } from './npm.mjs';

/** First line of stdout, trimmed. Anything else is not a version. */
function firstLine(stdout) {
  const line = String(stdout ?? '')
    .split('\n')
    .map((value) => value.trim())
    .find((value) => value.length > 0);
  return line && line.length > 0 ? line : null;
}

export function installedVersion(service, { timeoutMs = 15000 } = {}) {
  return readInstalledVersion(service, { timeoutMs }).version;
}

/**
 * The installed version as a reading: the version, or null with the reason
 * nothing could be read. installedVersion() folds a failure into null, which
 * is right for a version shown beside the service and wrong for anything that
 * acts on it. A version command can fail for reasons that say nothing about
 * what is installed, a timeout or one that needs the service up to answer, and
 * recovery must never take that for a version that is neither of the ones an
 * update noted.
 */
export function readInstalledVersion(service, { timeoutMs = 15000 } = {}) {
  if (!service.installedVersion) return { version: null, error: `${service.name} has no installedVersion command` };
  const result = runArgv(service.installedVersion, { timeoutMs });
  if (!result.ok) return { version: null, error: describeFailure(result) };
  const version = firstLine(result.stdout);
  return version ? { version, error: null } : { version: null, error: `${result.command} printed no version` };
}

/**
 * What the service says the newest version is. Any failure degrades to null,
 * which callers must read as "cannot tell", never as "nothing new".
 */
export function fetchLatestVersion(service, { timeoutMs = 20000 } = {}) {
  if (!service.latestVersion) return { version: null, error: null };
  const result = runArgv(service.latestVersion, { timeoutMs });
  if (!result.ok) return { version: null, error: describeFailure(result) };
  const version = firstLine(result.stdout);
  if (!version) return { version: null, error: `${result.command} printed no version` };
  const pattern = compilePattern(service.versionPattern, null);
  if (pattern && !pattern.test(version)) {
    return { version: null, error: `unexpected version: ${JSON.stringify(version)}` };
  }
  return { version, error: null };
}

/**
 * The same 10 minute cache the other kinds use. A latestVersion command can be a
 * network call, and `status` has a budget it has to stay inside.
 */
export function cachedLatestVersion(service, options = {}) {
  if (!service.latestVersion) return { version: null, cached: false, error: null };
  return cachedLookup(channelCacheKey(service, 'command'), options, () =>
    fetchLatestVersion(service, { timeoutMs: options.timeoutMs ?? 4000 }),
  );
}

/**
 * Run the update command. It changes the machine, so it goes through the
 * mutation runner: a deadline stops everything it started before anything
 * after it, a rollback included, may begin.
 */
export async function update(service, { timeoutMs = 600000 } = {}) {
  if (!service.update) return { ok: false, message: `${service.name} has no update command configured` };
  const result = await runMutationArgv(service.update, { timeoutMs });
  return { ok: result.ok, message: result.ok ? null : describeFailure(result), result };
}
