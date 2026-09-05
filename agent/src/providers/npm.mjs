// Services installed globally with npm: which version is on disk, which version
// the dist-tag points at, pre-warming the cache, and the install itself.
//
// Everything here is parametrized by the service object. The package name, the
// dist-tag, the prefix, the allowed install scripts and the pattern a resolved
// version has to match all come from config.json.

import fs from 'node:fs';
import path from 'node:path';
import {
  compilePattern,
  DEFAULT_VERSION_PATTERN,
  defaultNpmPrefix,
  describeFailure,
  globalModulesDir,
  runCommand,
} from '../config.mjs';
import { loadCache, saveCache } from '../state.mjs';

const LATEST_CACHE_MS = 10 * 60 * 1000;
const PREFIX_PROBE_INTERVAL_MS = 10 * 60 * 1000;

function packageJsonPath(service, prefix) {
  return path.join(globalModulesDir(prefix), service.package, 'package.json');
}

/** The version installed under one specific prefix, with no discovery involved. */
export function installedVersionAt(service, prefix) {
  try {
    const raw = fs.readFileSync(packageJsonPath(service, prefix), 'utf8');
    const version = JSON.parse(raw)?.version;
    return typeof version === 'string' && version.length > 0 ? version : null;
  } catch {
    return null;
  }
}

/**
 * How to run npm without relying on a shell. On Windows the npm launcher is a
 * .cmd file, which Node refuses to spawn without a shell, so we prefer calling
 * npm's own JS entry point with the running Node binary. That also guarantees
 * npm runs on the same Node we are running on.
 */
export function npmInvocation(prefix, { platform = process.platform, nodePath = process.execPath, searchPath = process.env.PATH ?? '' } = {}) {
  const nodeDir = path.dirname(nodePath);
  const candidates = [
    // Windows layout: C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js
    path.join(nodeDir, 'node_modules', 'npm', 'bin', 'npm-cli.js'),
    // Linux layout: /usr/bin/node -> /usr/lib/node_modules/npm/bin/npm-cli.js
    path.join(nodeDir, '..', 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js'),
  ];
  if (prefix) candidates.push(path.join(globalModulesDir(prefix), 'npm', 'bin', 'npm-cli.js'));

  if (platform === 'win32') {
    // npm may belong to a different Node installation on PATH. Resolve its
    // launcher as a file, then inspect the standard adjacent JavaScript entry;
    // never ask cmd.exe to interpret package names or --prefix arguments.
    for (const raw of searchPath.split(';')) {
      const directory = raw.replace(/^"(.*)"$/, '$1');
      if (!path.isAbsolute(directory)) continue;
      const launcher = path.join(directory, 'npm.cmd');
      try {
        if (!fs.statSync(launcher).isFile()) continue;
        for (const location of [launcher, fs.realpathSync(launcher)]) {
          candidates.push(path.join(path.dirname(location), 'node_modules', 'npm', 'bin', 'npm-cli.js'));
        }
      } catch { /* this PATH entry does not contain an accessible launcher */ }
    }
  }

  for (const candidate of candidates) {
    try {
      if (fs.statSync(candidate).isFile()) return { file: nodePath, lead: [candidate], shell: false };
    } catch {
      /* try the next candidate */
    }
  }

  if (platform === 'win32') return {
    file: null, lead: [], shell: false,
    error: 'npm-cli.js could not be located beside Node, the npm prefix, or an npm.cmd launcher on PATH; install npm with Node before retrying',
  };
  return { file: 'npm', lead: [], shell: false };
}

export function runNpm(args, options = {}) {
  const invocation = npmInvocation(options.prefix);
  if (invocation.error) return {
    ok: false, code: null, signal: null, stdout: '', stderr: '', timedOut: false,
    error: invocation.error, command: 'npm',
  };
  return runCommand(invocation.file, [...invocation.lead, ...args], {
    timeoutMs: options.timeoutMs ?? 60000,
    shell: invocation.shell,
  });
}

/**
 * The npm prefix that holds the global install. The per-platform default is
 * normally right; the probe only exists so a changed npm config does not
 * silently make us report "not installed". Probe results are cached in
 * state.json, and a failed probe is rate limited so `status` stays fast.
 */
export function resolveNpmPrefix(service, { allowProbe = true, readOnly = false } = {}) {
  if (typeof service.npmPrefix === 'string' && service.npmPrefix.length > 0) return service.npmPrefix;

  const fallback = defaultNpmPrefix();
  if (installedVersionAt(service, fallback) !== null) return fallback;

  const state = loadCache({ readOnly });
  // The first version of the agent kept one prefix at the top level, because it
  // only ever looked after one package. Both are read; only the keyed form is
  // written from here on.
  const cached = state.npmPrefixes?.[service.id] ?? state.npmPrefix;
  if (typeof cached === 'string' && installedVersionAt(service, cached) !== null) return cached;

  const lastProbe = Date.parse(state.npmPrefixProbedAt ?? '');
  const probedRecently = !Number.isNaN(lastProbe) && Date.now() - lastProbe < PREFIX_PROBE_INTERVAL_MS;
  if (!allowProbe || probedRecently) return fallback;

  const result = runNpm(['prefix', '--global'], { timeoutMs: 15000 });
  const probed = result.ok ? result.stdout.trim().split('\n').pop()?.trim() : '';
  saveCache((current) => ({
    npmPrefixProbedAt: new Date().toISOString(),
    npmPrefixes: { ...(current.npmPrefixes ?? {}), [service.id]: probed && probed.length > 0 ? probed : cached ?? null },
  }));
  return probed && probed.length > 0 ? probed : fallback;
}

/** The version currently on disk, or null when the package is not installed here. */
export function installedVersion(service, options = {}) {
  return installedVersionAt(service, resolveNpmPrefix(service, options));
}

/** Where npm installed the package; a running server's command line contains it. */
export function packageRoot(service, { allowProbe = false, readOnly = false } = {}) {
  return path.join(globalModulesDir(resolveNpmPrefix(service, { allowProbe, readOnly })), service.package);
}

/** The pattern a resolved version has to match before it is handed to npm install. */
export function versionPattern(service) {
  return compilePattern(service.versionPattern, DEFAULT_VERSION_PATTERN);
}

/**
 * Ask the registry what the channel dist-tag points at. Returns null on any
 * failure (offline, timeout, garbage output) — callers must treat null as
 * "unknown", never as "no update".
 */
export function fetchLatestVersion(service, { timeoutMs = 20000 } = {}) {
  const channel = service.channel || 'latest';
  const result = runNpm(['view', `${service.package}@${channel}`, 'version', '--json'], {
    timeoutMs,
    prefix: service.npmPrefix,
  });
  if (!result.ok) return { version: null, error: describeFailure(result) };

  let parsed;
  try {
    parsed = JSON.parse(result.stdout.trim());
  } catch {
    return { version: null, error: 'npm view returned output that is not JSON' };
  }
  // A dist-tag normally resolves to one version, but npm answers with an array
  // when several versions match; the newest is last.
  const version = Array.isArray(parsed) ? parsed[parsed.length - 1] : parsed;
  const pattern = versionPattern(service);
  if (typeof version !== 'string' || (pattern && !pattern.test(version))) {
    return { version: null, error: `unexpected ${channel} version: ${JSON.stringify(version)}` };
  }
  return { version, error: null };
}

/** The state.json key a service's channel lookup is cached under. */
export function channelCacheKey(service, source) {
  return `${source}:${service.id}:${service.channel || 'latest'}`;
}

function readChannelCache(key, maxAgeMs, readOnly = false) {
  const cache = loadCache({ readOnly }).channelCache;
  const entry = cache?.[key];
  const cachedAt = Date.parse(entry?.at ?? '');
  if (entry && typeof entry.version === 'string' && !Number.isNaN(cachedAt) && Date.now() - cachedAt < maxAgeMs) {
    return entry.version;
  }
  return null;
}

export function writeChannelCache(key, version, options = {}) {
  // Computed inside the transaction, so two services caching a lookup at the
  // same moment cannot drop each other's entry.
  saveCache((current) => {
    const cache = current.channelCache;
    const kept = {};
    // The first version of the agent kept one flat entry here rather than a map,
    // because it only ever looked up one thing. Its leftover keys are dropped on
    // the first write instead of being carried around forever.
    if (cache && typeof cache === 'object' && !Array.isArray(cache)) {
      for (const [name, entry] of Object.entries(cache)) {
        if (entry && typeof entry === 'object' && typeof entry.version === 'string') kept[name] = entry;
      }
    }
    return { channelCache: { ...kept, [key]: { version, at: new Date().toISOString() } } };
  }, options);
}

export function cachedLookup(key, { maxAgeMs = LATEST_CACHE_MS, readOnly = false, cacheWaitMs } = {}, fetcher) {
  const hit = readChannelCache(key, maxAgeMs, readOnly);
  if (hit) return { version: hit, cached: true, error: null };
  const fresh = fetcher();
  if (fresh && typeof fresh.then === 'function') {
    return fresh.then((value) => {
      if (value.version) writeChannelCache(key, value.version, { skipMigration: readOnly, waitMs: cacheWaitMs });
      return { ...value, cached: false };
    });
  }
  if (fresh.version) writeChannelCache(key, fresh.version, { skipMigration: readOnly, waitMs: cacheWaitMs });
  return { ...fresh, cached: false };
}

/**
 * Channel version with a 10 minute cache in state.json, used by `status`.
 * Writing the cache is the one piece of state `status` touches; it never changes
 * anything about the machine, and a failed write is ignored.
 */
export function cachedLatestVersion(service, { timeoutMs = 4000, maxAgeMs = LATEST_CACHE_MS } = {}) {
  return cachedLookup(channelCacheKey(service, 'npm'), { maxAgeMs }, () =>
    fetchLatestVersion(service, { timeoutMs }),
  );
}

/** Pre-warm the npm cache so the window with the service stopped stays short. */
export function warmCache(service, version, attempts = 3) {
  let last = null;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    last = runNpm(['cache', 'add', `${service.package}@${version}`], {
      timeoutMs: 180000,
      prefix: service.npmPrefix,
    });
    if (last.ok) return { ok: true, attempts: attempt, message: null };
  }
  return { ok: false, attempts, message: last ? describeFailure(last) : 'npm cache add did not run' };
}

/** npm's own major version, or null when it cannot be determined. */
function npmMajor(prefix) {
  const result = runNpm(['--version'], { timeoutMs: 20000, prefix });
  if (!result.ok) return null;
  const match = /^(\d+)\./.exec(result.stdout.trim());
  return match ? Number.parseInt(match[1], 10) : null;
}

/**
 * npm 12 stopped running package install scripts unless they are allowlisted.
 * A dependency with no prebuild for this platform has to compile during install,
 * and without the allowlist the install reports success and then every start
 * dies on the module it could not build.
 *
 * The flag does not exist before npm 12 and older npm treats an unknown flag as
 * a package name, so it is only added when npm is actually new enough. That is
 * why this is version-gated rather than passed unconditionally: one machine can
 * be on npm 12 while another is still on npm 11.
 */
function allowScriptsArgs(service, prefix) {
  const packages = Array.isArray(service.allowScripts) ? service.allowScripts.filter(Boolean) : [];
  if (packages.length === 0) return [];
  const major = npmMajor(prefix);
  if (major === null || major < 12) return [];
  return [`--allow-scripts=${packages.join(',')}`];
}

export function installVersion(service, version, prefix) {
  const result = runNpm(
    [
      'install',
      '--global',
      '--prefix',
      prefix,
      '--no-audit',
      '--no-fund',
      ...allowScriptsArgs(service, prefix),
      `${service.package}@${version}`,
    ],
    { timeoutMs: 600000, prefix },
  );
  return { ok: result.ok, message: result.ok ? null : describeFailure(result), result };
}
