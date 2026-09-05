// Conservative probes for explicitly enabled product profiles. Process rows
// stay in this process; replies never include command lines or credentials.
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { resolveClaudeUpdatePolicy, claudeTargetAllowed, compareProfileVersions, nativeClaudeVersionAt } from './service-profile-claude.mjs';
import { readDesktopMetadata } from './service-profile-desktop.mjs';

const NAMES = {
  'claude-code': ['claude', 'claude-code'],
  'claude-desktop': ['Claude', 'claude-desktop'],
  'codex-cli': ['codex', 'codex-cli'],
  'codex-desktop': ['Codex', 'ChatGPT'],
  'chatgpt-desktop': ['ChatGPT'],
  opencode: ['opencode', 'opencode-cli'],
  antigravity: ['Antigravity', 'antigravity'],
};

export function processExitVerdict(profile, rows, ownPid = process.pid) {
  const names = NAMES[profile];
  if (!names || !Array.isArray(rows)) return { busy: true, unknown: true, reason: 'Process inspection is unavailable for this profile.' };
  const patterns = names.map((name) => new RegExp(`(?:^|[\\s/\\\\"'])${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}(?:\\.exe|\\.app)?(?:$|[\\s/\\\\"'])`, 'i'));
  const firstWord = (text) => {
    const match = /^\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+|$)/.exec(text);
    return match ? { value: match[1] ?? match[2] ?? match[3], rest: text.slice(match[0].length) } : null;
  };
  let uncertain = false;
  for (const row of rows) {
    if (!Number.isInteger(row.pid) || row.pid <= 0) { uncertain = true; continue; }
    if (row.pid === ownPid) continue;
    const name = String(row.name ?? '');
    const command = typeof row.command === 'string' ? row.command : '';
    if (!name || !command) uncertain = true;
    const executable = firstWord(command);
    const identities = [name, executable?.value ?? ''];
    const runtime = [name, executable?.value ?? ''].some((identity) => /^(node|bun|electron)(\.exe)?$/i.test(identity.split(/[\\/]/).at(-1)));
    if (runtime) {
      const scriptTail = command.startsWith(`${name} `) ? command.slice(name.length).trimStart() : executable?.rest;
      const script = scriptTail && firstWord(scriptTail);
      if (!script || script.value.startsWith('-')) uncertain = true;
      else {
        identities.push(script.value);
        if (!/\.(?:[cm]?js|[cm]?ts)$/i.test(script.value)) uncertain = true;
      }
    }
    if (patterns.some((pattern) => identities.some((identity) => pattern.test(identity)))) {
      return { busy: true, unknown: false, reason: 'A related application or CLI session is still running.' };
    }
    if (!command && runtime) uncertain = true;
  }
  return uncertain
    ? { busy: true, unknown: true, reason: 'Some process identities could not be inspected; waiting for a reliable idle result.' }
    : { busy: false, unknown: false, reason: 'No related application or CLI session is running.' };
}

export function inspectProfileProcesses(profile, { platform = process.platform, run = spawnSync } = {}) {
  if (!NAMES[profile]) return processExitVerdict(profile, null);
  try {
    let rows;
    if (platform === 'win32') {
      const result = run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', "$ErrorActionPreference='Stop'; @(Get-CimInstance Win32_Process | Select-Object ProcessId,Name,CommandLine) | ConvertTo-Json -Compress"], { encoding: 'utf8', timeout: 4000, maxBuffer: 4 * 1024 * 1024, windowsHide: true });
      if (result.status !== 0 || result.error) return processExitVerdict(profile, null);
      const entries = JSON.parse(result.stdout);
      if (!Array.isArray(entries)) return processExitVerdict(profile, null);
      rows = entries.map((row) => ({ pid: Number(row.ProcessId), name: row.Name, command: row.CommandLine }));
    } else if (platform === 'darwin' || platform === 'linux') {
      // ps does not quote fields. Separate PID-prefixed reads preserve spaces
      // in comm instead of guessing where one column ends and another begins.
      const readColumn = (column) => {
        const result = run('/bin/ps', ['-ww', '-ax', '-o', 'pid=', '-o', `${column}=`], { encoding: 'utf8', timeout: 2000, maxBuffer: 4 * 1024 * 1024 });
        if (result.status !== 0 || result.error || !String(result.stdout).trim()) return null;
        const entries = new Map();
        for (const line of String(result.stdout).trim().split('\n')) {
          const found = /^\s*(\d+)\s+(.*)$/.exec(line);
          if (!found || entries.has(Number(found[1]))) return null;
          entries.set(Number(found[1]), found[2]);
        }
        // Each short-lived ps child exists in its own snapshot only. Its PID
        // comes from spawnSync, so excluding it cannot hide a product session.
        if (Number.isInteger(result.pid)) entries.delete(result.pid);
        return entries;
      };
      const names = readColumn('comm'); const commands = readColumn('args');
      if (!names || !commands) return processExitVerdict(profile, null);
      rows = [...new Set([...names.keys(), ...commands.keys()])].map((pid) => ({ pid, name: names.get(pid), command: commands.get(pid) }));
    } else return processExitVerdict(profile, null);
    return processExitVerdict(profile, rows);
  } catch { return processExitVerdict(profile, null); }
}

export function readProfileVersion(executable, { run = spawnSync, cwd } = {}) {
  if (typeof executable !== 'string' || !path.isAbsolute(executable)) return null;
  const result = run(executable, ['--version'], { encoding: 'utf8', timeout: 4000, maxBuffer: 64 * 1024, windowsHide: true, cwd });
  if (result.status !== 0 || result.error) return null;
  return /(?:^|\s)(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)(?=\s|$)/.exec(String(result.stdout))?.[1] ?? null;
}

/** The fixed publisher feed used by the official native installer. */
export async function latestClaudeVersion(channel, { request = fetch } = {}) {
  if (!['latest', 'stable'].includes(channel)) return null;
  try {
    const response = await request(`https://downloads.claude.ai/claude-code-releases/${channel}`, { signal: AbortSignal.timeout(5000), redirect: 'error' });
    if (!response.ok || !response.body) return null;
    const reader = response.body.getReader();
    let total = 0;
    const chunks = [];
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        total += value.byteLength;
        if (total > 4096) { await reader.cancel(); return null; }
        chunks.push(Buffer.from(value));
      }
    } finally { reader.releaseLock(); }
    const version = Buffer.concat(chunks).toString('utf8').trim();
    return /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version) ? version : null;
  } catch { return null; }
}

export async function runNativeClaudeProbe(verb, { executable, home, expectedPolicy }, { resolvePolicy = resolveClaudeUpdatePolicy, run = spawnSync, latest = latestClaudeVersion, nativeVersion = nativeClaudeVersionAt } = {}) {
  const ownedVersion = nativeVersion(executable);
  const supported = compareProfileVersions(ownedVersion, '2.1.251');
  if (supported === null || supported < 0) return { ok: false, message: 'The selected executable is no longer a supported native installation; review the profile again.' };
  const policy = resolvePolicy({ home, executable });
  if (!policy.ok || policy.hash !== expectedPolicy || typeof executable !== 'string' || !path.isAbsolute(executable)) {
    return { ok: false, message: 'Native update policy changed or could not be verified; review the profile again.' };
  }
  if (verb === 'native-version') {
    const version = readProfileVersion(executable, { cwd: home, run });
    return version === ownedVersion ? { ok: true, version } : { ok: false, message: 'The installed native version could not be read.' };
  }
  if (verb === 'native-latest') {
    const installed = readProfileVersion(executable, { cwd: home, run });
    const version = await latest(policy.channel);
    return installed === ownedVersion && claudeTargetAllowed(policy, version, installed) ? { ok: true, version } : { ok: false, message: 'No verified target satisfies the current channel and version constraints.' };
  }
  if (verb !== 'native-update') return { ok: false, message: 'Unsupported native probe.' };
  try {
    const installed = readProfileVersion(executable, { cwd: home, run });
    const target = await latest(policy.channel);
    if (installed !== ownedVersion || !claudeTargetAllowed(policy, target, installed)) return { ok: false, message: 'No verified target satisfies the current channel and version constraints.' };
    const currentPolicy = resolvePolicy({ home, executable });
    if (!currentPolicy.ok || currentPolicy.hash !== expectedPolicy || nativeVersion(executable) !== ownedVersion) return { ok: false, message: 'Native update policy or installation changed; review the profile again.' };
    const result = run(executable, ['update'], { cwd: home, encoding: 'utf8', timeout: 540000, maxBuffer: 1024 * 1024, windowsHide: true });
    if (result.status !== 0 || result.error) return { ok: false, message: 'The official native updater did not complete successfully.' };
    const afterPolicy = resolvePolicy({ home, executable });
    const observed = readProfileVersion(executable, { cwd: home, run });
    return afterPolicy.ok && afterPolicy.hash === expectedPolicy && observed === target && nativeVersion(executable) === target
      ? { ok: true }
      : { ok: false, message: 'The native update did not match the verified target and policy; refresh before retrying.' };
  } catch { return { ok: false, message: 'The official native updater could not be started.' }; }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [verb, profile, ...extra] = process.argv.slice(2);
  if (verb === 'desktop-version' && ['claude-desktop', 'codex-desktop', 'chatgpt-desktop', 'antigravity'].includes(profile) && extra.length === 2) {
    const metadata = readDesktopMetadata(extra[0]);
    if (!metadata || metadata.identity !== extra[1]) { process.stderr.write('The installed application metadata could not be verified.\n'); process.exitCode = 1; }
    else process.stdout.write(`${metadata.version}\n`);
  } else if (['native-version', 'native-latest', 'native-update'].includes(verb) && profile === 'claude-code' && extra.length === 3) {
    const [executable, home, expectedPolicy] = extra;
    const result = await runNativeClaudeProbe(verb, { executable, home, expectedPolicy });
    if (!result.ok) { process.stderr.write(`${result.message}\n`); process.exitCode = 1; }
    else if (result.version) process.stdout.write(`${result.version}\n`);
  } else if (verb !== 'busy' || extra.length || !NAMES[profile]) {
    process.stderr.write('Unsupported profile probe.\n'); process.exitCode = 1;
  } else process.stdout.write(`${JSON.stringify(inspectProfileProcesses(profile))}\n`);
}
